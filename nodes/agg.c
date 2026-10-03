#include "postgres.h"

#include "access/htup_details.h"
#include "access/nbtree.h"
#include "catalog/pg_aggregate.h"
#include "commands/explain_format.h"
#include "common/hashfn.h"
#include "common/int.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "parser/parse_agg.h"
#include "utils/array.h"
#include "utils/float.h"
#include "utils/fmgroids.h"
#include "utils/syscache.h"
#include "utils/typcache.h"
#include "utils/lsyscache.h"
#include "utils/datum.h"
#include "utils/builtins.h"
#include "utils/numeric.h"
#include "utils/pg_locale.h"
#include "utils/regproc.h"
#include "utils/ruleutils.h"

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"
#include "agg.h"
#include "agg_node.h"

/*
 * TessAgg computes the aggregates of a query over the batches of a batch
 * child, so that no row is handed up one at a time. Without GROUP BY it
 * stands in for the core's plain Aggregate and returns the one result
 * row: each aggregate is computed per batch by the registered batch
 * function of its aggregate (tessera/function.h, kind
 * TESS_FUNCTION_AGGREGATE) and the partials are combined here, with the
 * overflow check the core's transition would make. With GROUP BY it
 * stands in for the core's HashAggregate: each row finds the record of
 * its keys in a hash table (tessera/table.h), whose payload holds the
 * group's aggregate states, and the groups go out in batches when the
 * input ends. See docs/nodes.md.
 */
/* A batch with at most this many survivors is gathered for one call later. */
#define AGG_GATHER_ROWS 8

/* The counters every participant of a parallel plan shares. */
enum
{
	AGG_BATCHES,
	AGG_ROWS,
	AGG_CALLS,
	AGG_COMPUTED,
	AGG_GROUPS,
	AGG_MEMORY,
	AGG_GROWS,
	/*
	 * Spilling: the most partitions of a level, the partitions sent to
	 * disk while the input was read, the chunks and bytes written, the
	 * partitions split into a level below.
	 */
	AGG_PARTITIONS,
	AGG_EVICTIONS,
	AGG_SPILLED,
	AGG_DISK,
	AGG_SPLITS,
	/* Partial mode: the times the groups went out before the input ended. */
	AGG_EARLY,
	/* Generic states past hash_mem: the rows sent to partitions on disk. */
	AGG_SPILLED_ROWS,
	AGG_NCOUNTERS
};

static const CustomExecMethods agg_exec_methods;
static const TessBatchOps groups_batch_ops;

static void
agg_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessAggState *state = (TessAggState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	TessBuilderConfig builder = TESS_STRUCT_INITIALIZER(TessBuilderConfig);
	TupleTableSlot *result = css->ss.ps.ps_ResultTupleSlot;
	Plan	   *child_plan = linitial(cscan->custom_plans);
	Bitmapset  *projection = NULL;
	List	   *computed = NIL;
	List	   *arguments;
	List	   *more;
	List	   *filters;
	List	   *eqops;
	List	   *keys;
	TessPlanReader *reader;
	int			groups;
	int			index = 0;

	tess_node_require_forward(eflags, "TessAgg");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_agg_node || info.nchildren < 1 || info.nchildren > 2 ||
		info.child_names[0] == NULL || cscan->custom_scan_tlist == NIL)
		elog(ERROR, "TessAgg received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, TESS_AGG_DATA,
									 TESS_AGG_DATA_VERSION);
	arguments = tess_plan_read_list(reader, "arguments");
	more = tess_plan_read_list(reader, "more");
	filters = tess_plan_read_list(reader, "filters");
	eqops = tess_plan_read_int_list(reader, "key_eqops");
	keys = tess_plan_read_list(reader, "keys");
	groups = tess_plan_read_int(reader, "groups");
	state->groups_estimate = (uint64) Max(groups, 0);
	state->setop = tess_plan_read_int(reader, "setop");
	state->partial = tess_plan_read_int(reader, "partial") != 0;
	state->own_states = tess_plan_read_int(reader, "own_states") != 0;
	state->finalize = tess_plan_read_int(reader, "finalize") != 0;
	tess_plan_reader_finish(reader);
	if (state->finalize && (state->partial || state->setop >= 0))
		elog(ERROR, "TessAgg received a foreign plan");
	if (state->own_states && !state->partial && !(state->finalize && keys == NIL))
		elog(ERROR, "TessAgg received a foreign plan");
	if ((state->setop >= 0) != (info.nchildren == 2) ||
		(state->setop >= 0 && (list_length(arguments) != 2 || list_length(keys) == 0)))
		elog(ERROR, "TessAgg received a foreign plan");
	state->nkeys = list_length(keys);
	if (state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(arguments) + state->nkeys != list_length(cscan->custom_scan_tlist) ||
		(state->nkeys == 0 && arguments == NIL) ||
		list_length(arguments) > AGG_MAX_GROUPED ||
		list_length(filters) != list_length(arguments) ||
		list_length(eqops) != state->nkeys)
		elog(ERROR, "TessAgg received a foreign plan");
	state->child = ExecInitNode(child_plan, estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->input = tess_input_create(estate->es_query_cxt, state->child);
	state->child_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	tess_plan_get_layout(child_plan, &state->child_layout);
	if (state->setop >= 0)
	{
		Plan	   *right = lsecond(cscan->custom_plans);

		state->sides[0] = state->child;
		state->side_inputs[0] = state->input;
		state->side_layouts[0] = state->child_layout;
		state->sides[1] = ExecInitNode(right, estate, eflags);
		css->custom_ps = lappend(css->custom_ps, state->sides[1]);
		state->side_inputs[1] = tess_input_create(estate->es_query_cxt, state->sides[1]);
		state->side_layouts[1] = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
		tess_plan_get_layout(right, &state->side_layouts[1]);
		state->setop_copies = -1;
	}
	state->nvalues = list_length(arguments);
	state->values = palloc0_array(AggValue, Max(state->nvalues, 1));
	state->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	/* The keys are the first computed columns, the table's key kinds. */
	foreach_ptr(Node, key, keys)
	{
		int			position = foreach_current_index(key);

		if (list_nth_int(eqops, position) != 0)
		{
			state->kinds[position] = TESS_TABLE_KEY_INT8;
			state->dicts[position] = agg_key_dict_create(state, (Oid) list_nth_int(eqops, position),
													exprType(key), exprCollation(key));
			state->has_dicts = true;
			state->has_forms |= state->dicts[position]->forms;
			state->row_spill = true;
		}
		else if (!tess_word_key_kind(exprType(key), &state->kinds[position]))
			elog(ERROR, "TessAgg received a key of type %u", exprType(key));
		computed = lappend(computed,
						   makeTargetEntry((Expr *) key, position + 1, NULL, false));
		foreach_ptr(Var, var, pull_var_clause(key, 0))
		{
			int			column = var->varno == INDEX_VAR ?
				tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

			if (column < 0)
				elog(ERROR, "TessAgg grouping expression names no column of its child");
			projection = bms_add_member(projection, column);
		}
	}
	foreach_ptr(TargetEntry, entry, cscan->custom_scan_tlist)
	{
		Aggref	   *agg;
		AggValue   *value;

		if (foreach_current_index(entry) < state->nkeys)
			continue;
		agg = castNode(Aggref, entry->expr);
		value = &state->values[index++];

		/* The partial values are the whole ones' types: nothing to convert. */
		if (DO_AGGSPLIT_SKIPFINAL(agg->aggsplit) != state->partial)
			elog(ERROR, "TessAgg received a foreign plan");
		value->kind = aggregate_kind(agg->aggfnoid);
		value->wide = agg->aggtranstype == INT8OID;
		value->function = tess_runtime_api()->functions->find(agg->aggfnoid);
		value->computed = -1;
		value->filter = -1;
		if (!batch_aggregate(agg))
		{
			if (!generic_supported(agg) ||
				(state->nkeys > 0 && DO_AGGSPLIT_SKIPFINAL(agg->aggsplit) &&
				 !(state->own_states && sum_state_aggregate(agg))))
				elog(ERROR, "TessAgg has no implementation of %s",
					 format_procedure(agg->aggfnoid));
			value->kind = AGG_GENERIC;
			value->generic = agg_generic_init(state, agg);
			if (state->nkeys > 0 && state->generic_output == NULL)
			{
				state->has_generic = true;
				state->row_spill = true;
				state->generic_output = AllocSetContextCreate(estate->es_query_cxt,
															  "TessAgg generic values",
															  ALLOCSET_DEFAULT_SIZES);
			}
		}
		/*
		 * A partial value is the transition type's: an extreme's, int8 for
		 * the rest; with groups a sum state's the node's own
		 * (TessTableSumInput), without them a generic one's the core's,
		 * which its combine function takes.
		 */
		if (state->finalize && ((value->kind == AGG_GENERIC && state->nkeys > 0 &&
								 !value->generic->sum_state) ||
								agg->aggdistinct != NIL))
			elog(ERROR, "TessAgg received a foreign plan");
		if (state->partial && value->kind == AGG_GENERIC && state->nkeys > 0 &&
			!value->generic->sum_state)
			elog(ERROR, "TessAgg received a foreign plan");
		switch (value->kind)
		{
			case AGG_COUNT:
				value->accumulate = state->finalize ? TESS_TABLE_SUM_INT8 :
					agg->args == NIL ? TESS_TABLE_COUNT_ROWS : TESS_TABLE_COUNT;
				break;
			case AGG_SUM:
				value->accumulate = state->finalize ? TESS_TABLE_SUM_INT8 :
					TESS_TABLE_SUM_INT4;
				break;
			case AGG_MIN:
				value->accumulate = value->wide ? TESS_TABLE_MIN_INT8 :
					TESS_TABLE_MIN_INT4;
				break;
			case AGG_MAX:
				value->accumulate = value->wide ? TESS_TABLE_MAX_INT8 :
					TESS_TABLE_MAX_INT4;
				break;
			case AGG_GENERIC:
				break;
		}
		if (agg->args != NIL || state->finalize)
		{
			Node	   *argument = list_nth(arguments, index - 1);
			List	   *vars = pull_var_clause(argument, 0);

			value->computed = list_length(computed);
			computed = lappend(computed,
							   makeTargetEntry((Expr *) argument,
											   value->computed + 1, NULL, false));
			/* The other arguments of a generic aggregate follow the first. */
			foreach_ptr(Node, other, (List *) list_nth(more, index - 1))
			{
				computed = lappend(computed,
								   makeTargetEntry((Expr *) other, list_length(computed) + 1,
												   NULL, false));
				vars = list_concat(vars, pull_var_clause(other, 0));
			}
			foreach_ptr(Var, var, vars)
			{
				int			column = var->varno == INDEX_VAR ?
					tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

				if (column < 0)
					elog(ERROR, "TessAgg argument names no column of its child");
				projection = bms_add_member(projection, column);
			}
			value->gathered_values = palloc_array(Datum, 64);
			value->gathered_isnull = palloc_array(bool, 64);
			if (agg->aggdistinct != NIL)
			{
				SortGroupClause *clause = linitial_node(SortGroupClause, agg->aggdistinct);

				if (!tess_word_key_kind(exprType(argument), &value->argument_kind))
				{
					value->argument_kind = TESS_TABLE_KEY_INT8;
					value->distinct_dict = agg_key_dict_create(state, clause->eqop,
														   exprType(argument),
														   exprCollation(argument));
					/* Only the count of the values matters, not a form to put out. */
					value->distinct_dict->forms = false;
				}
				value->distinct = palloc0(sizeof(struct DistinctSet));
				state->has_distinct = true;
			}
		}
	}
	/* The FILTER conditions, computed columns after every argument. */
	foreach_ptr(Node, filter, filters)
	{
		AggValue   *value = &state->values[foreach_current_index(filter)];

		if (filter == NULL)
			continue;
		value->filter = list_length(computed);
		computed = lappend(computed,
						   makeTargetEntry((Expr *) filter, value->filter + 1, NULL, false));
		foreach_ptr(Var, var, pull_var_clause(filter, 0))
		{
			int			column = var->varno == INDEX_VAR ?
				tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

			if (column < 0)
				elog(ERROR, "TessAgg filter names no column of its child");
			projection = bms_add_member(projection, column);
		}
	}
	state->ncomputed = list_length(computed);
	state->computed_lens = palloc_array(int16, Max(state->ncomputed, 1));
	state->computed_byvals = palloc_array(bool, Max(state->ncomputed, 1));
	state->computed_columns = palloc0_array(TessDatumColumn, Max(state->ncomputed, 1));
	foreach_node(TargetEntry, entry, computed)
	{
		int			column = foreach_current_index(entry);

		get_typlenbyval(exprType((Node *) entry->expr), &state->computed_lens[column],
						&state->computed_byvals[column]);
	}
	if (computed != NIL)
	{
		/*
		 * The arguments are computed columns over the child's target list,
		 * in a slot of its own: the node's scan tuple is the aggregates'.
		 */
		state->projection =
			tess_node_projection(css,
								 ExecInitExtraTupleSlot(estate,
														ExecTypeFromTL(child_plan->targetlist),
														&TTSOpsVirtual),
								 &state->child_layout, state->child_layout.ncolumns, computed);
		/*
		 * The right side: the same keys, its columns by position, and its
		 * side a constant 1 where the left side's is 0.
		 */
		if (state->setop >= 0)
		{
			List	   *right = copyObject(computed);
			TargetEntry *side = list_nth_node(TargetEntry, right, state->values[1].computed);
			Plan	   *plan = lsecond(cscan->custom_plans);
			Bitmapset  *columns = NULL;
			TessRequest side_request = TESS_STRUCT_INITIALIZER(TessRequest);

			if (!IsA(side->expr, Const))
				elog(ERROR, "TessAgg received a foreign plan");
			side->expr = (Expr *) makeConst(INT4OID, -1, InvalidOid, sizeof(int32),
											Int32GetDatum(1), false, true);
			state->side_projections[0] = state->projection;
			state->side_projections[1] =
				tess_node_projection(css,
									 ExecInitExtraTupleSlot(estate,
															ExecTypeFromTL(plan->targetlist),
															&TTSOpsVirtual),
									 &state->side_layouts[1], state->side_layouts[1].ncolumns,
									 right);
			foreach_node(TargetEntry, entry, right)
				foreach_node(Var, var, pull_var_clause((Node *) entry->expr, 0))
				{
					int			column = var->varno == INDEX_VAR ?
						tess_layout_column(&state->side_layouts[1], var->varattno - 1) : -1;

					if (column < 0)
						elog(ERROR, "TessAgg key names no column of its right side");
					columns = bms_add_member(columns, column);
				}
			side_request.projection_columns = columns;
			side_request.output_mode = TESS_OUTPUT_BATCH;
			tess_input_set_request(state->side_inputs[1], &side_request);
		}
	}
	/* Whole batches; the arguments' columns only for the surviving rows. */
	request.projection_columns = projection;
	/*
	 * Read in order only when the keys and the arguments, as computed, would
	 * first ask for a column before one they asked for already: else the
	 * provider walks each row once anyway, and the calls are wasted.
	 */
	{
		int			last = -1;
		bool		ascending = true;

		foreach_node(TargetEntry, entry, computed)
			foreach_node(Var, var, pull_var_clause((Node *) entry->expr, 0))
			{
				int			column = var->varno == INDEX_VAR ?
					tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

				if (column >= 0 && column < last)
					ascending = false;
				last = Max(last, column);
			}
		state->read_columns = palloc_array(int, Max(bms_num_members(projection), 1));
		/* Two sides of two layouts: their columns come as they are asked for. */
		for (int column = -1;
			 !ascending && state->setop < 0 &&
			 (column = bms_next_member(projection, column)) >= 0;)
			state->read_columns[state->nread_columns++] = column;
	}
	/* The states' places in a record: a word each, a sum or extreme state's more. */
	{
		int			slot = 1;

		for (int number = 0; number < state->nvalues; number++)
		{
			AggValue   *value = &state->values[number];

			value->slot = slot;
			slot += value->generic == NULL ? 1 :
				value->generic->sum_state ? AGG_SUM_STATE_WORDS :
				value->generic->extreme_state ? AGG_EXTREME_STATE_WORDS : 1;
		}
		state->payload_size = sizeof(uint64) * slot;
		state->sum_indexes = palloc_array(int, Max(state->nvalues, 1));
	}
	request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->input, &request);
	builder.parent_context = estate->es_query_cxt;
	builder.tuple_desc = result->tts_tupleDescriptor;
	builder.ncolumns = result->tts_tupleDescriptor->natts;
	builder.capacity = state->nkeys > 0 ? AGG_GROUP_ROWS : 1;
	if (state->nkeys > 0)
	{
		state->kernels = tess_runtime_kernels();
		if (state->kernels == NULL)
			elog(ERROR, "TessAgg needs the kernels module for GROUP BY");
		check(state, state->kernels->table_record_size(state->nkeys, state->payload_size,
													   &state->record_size,
													   &state->status));
		state->table_context = AllocSetContextCreate(estate->es_query_cxt,
													 "TessAgg groups",
													 ALLOCSET_DEFAULT_SIZES);
		/* For spilling: how the states merge, and the layout's fingerprint. */
		state->combines = palloc_array(TessTableCombine, Max(state->nvalues, 1));
		for (int value = 0; value < state->nvalues; value++)
			state->combines[value] =
				state->values[value].kind == AGG_COUNT ? TESS_TABLE_COMBINE_COUNT :
				state->values[value].kind == AGG_SUM ? TESS_TABLE_COMBINE_SUM :
				state->values[value].kind == AGG_MIN ? TESS_TABLE_COMBINE_MIN :
				TESS_TABLE_COMBINE_MAX;
		if (state->kernels->table_size(state->nkeys, state->kinds,
									   state->payload_size,
									   AGG_INITIAL_GROUPS, &state->layout_len,
									   &state->status) != TESS_OK)
			tess_status_report(&state->status);
		state->layout_index = palloc0(state->layout_len);
		if (state->kernels->table_create(state->layout_index, state->layout_len,
										 state->nkeys, state->kinds,
										 state->payload_size,
										 AGG_INITIAL_GROUPS, &state->status) != TESS_OK)
			tess_status_report(&state->status);
		state->state_words = palloc0_array(uint64,
										   Max(state->nvalues, 1) * AGG_GROUP_ROWS);
		if (state->projection == NULL)
			elog(ERROR, "TessAgg groups by computed columns");
	}
	/* DISTINCT in an aggregate keeps its pairs in tables, with or without groups. */
	if (state->has_distinct && state->kernels == NULL)
	{
		state->kernels = tess_runtime_kernels();
		if (state->kernels == NULL)
			elog(ERROR, "TessAgg needs the kernels module for DISTINCT");
	}
	state->builder = tess_builder_create(&builder);
	if (state->nkeys > 0 && state->setop < 0 && !state->has_generic &&
		css->ss.ps.qual == NULL && css->ss.ps.ps_ProjInfo == NULL)
	{
		state->direct = true;
		state->agg_values = palloc0_array(Datum, Max(state->nvalues, 1) * AGG_GROUP_ROWS);
		state->agg_isnull = palloc0_array(bool, Max(state->nvalues, 1) * AGG_GROUP_ROWS);
		state->groups_batch.abi_version = TESS_BATCH_ABI_VERSION;
		state->groups_batch.struct_size = sizeof(TessBatch);
		state->groups_batch.table_oid = InvalidOid;
		state->groups_batch.ops = &groups_batch_ops;
		state->groups_batch.private_data = state;
	}
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   result, &info.layout);
}

/*
 * A partial value into the running value, a batch's or, above a gather, a
 * participant's: counts and sums added as int8, 22003 past the range,
 * extremes compared.
 */
static void
join_partial(AggValue *value, Datum partial)
{
	switch (value->kind)
	{
		case AGG_COUNT:
		case AGG_SUM:
			if (pg_add_s64_overflow(value->total, DatumGetInt64(partial),
									&value->total))
				ereport(ERROR,
						(errcode(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
						 errmsg("bigint out of range")));
			break;
		case AGG_MIN:
		case AGG_MAX:
			{
				int64		found = value->wide ? DatumGetInt64(partial) :
					(int64) DatumGetInt32(partial);

				if (!value->has_value ||
					(value->kind == AGG_MIN ? found < value->extreme :
					 found > value->extreme))
					value->extreme = found;
				break;
			}
		case AGG_GENERIC:
			break;
	}
	value->has_value = true;
}

/*
 * One call of the aggregate's batch function over a column, or over rows
 * alone for count(*); the partial joins the running value. No readiness
 * mask: the column's rows are all initialized memory (tessera/batch.h),
 * and a mask would keep the kernel off its vector path for every word the
 * selection does not fill.
 */
static void
evaluate(TessAggState *state, AggValue *value, const TessDatumColumn *column,
		 TessRowMask *rows)
{
	TessFunctionCall call = TESS_STRUCT_INITIALIZER(TessFunctionCall);
	TessFunctionArg arg = TESS_STRUCT_INITIALIZER(TessFunctionArg);
	uint64		word = 0;
	TessRowMask present = {1, &word};
	Datum		partial = (Datum) 0;

	call.function = value->function;
	if (column != NULL)
	{
		arg.column = column;
		call.nargs = 1;
		call.args = &arg;
	}
	call.rows = rows;
	call.values = &partial;
	call.non_nulls = &present;
	call.context = CurrentMemoryContext;
	call.status = &state->status;
	state->calls++;
	if (value->function->evaluate(&call) != TESS_OK)
		tess_status_report(&state->status);
	if ((word & 1) == 0)
		return;
	join_partial(value, partial);
}

/* The gathered values as a column with every row selected, in one call. */
static void
flush_gathered(TessAggState *state, AggValue *value)
{
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	uint64		word;
	TessRowMask rows = {value->ngathered, &word};

	if (value->ngathered == 0)
		return;
	column.values = value->gathered_values;
	column.isnull = value->gathered_isnull;
	column.nrows = value->ngathered;
	word = value->ngathered == 64 ? UINT64_MAX :
		(UINT64CONST(1) << value->ngathered) - 1;
	evaluate(state, value, &column, &rows);
	value->ngathered = 0;
}

/*
 * The rows of those given that an aggregate's FILTER keeps: its computed
 * column is true there and NULL elsewhere (filter_value), asked for those
 * rows only; in the node's buffer, which the aggregate uses before the
 * next one asks.
 */
TessRowMask
agg_filtered_rows(TessAggState *state, TessBatch *batch, int filter, const TessRowMask *rows)
{
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	int			nwords = tess_row_mask_word_count(rows->nrows);
	int			row = -1;

	if (state->filter_words < nwords)
	{
		state->filter_bits = MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
												sizeof(uint64) * nwords);
		state->filter_words = nwords;
	}
	batch->ops->get_datum_column(batch, state->child_layout.ncolumns + filter, rows,
								 TESS_COLUMN_FOR_FILTER, &column);
	if (column.values == NULL || column.isnull == NULL || column.nrows != rows->nrows)
		elog(ERROR, "Tessera projection returned an invalid column");
	for (int word = 0; word < nwords; word++)
	{
		uint64		selected = rows->bits[word];
		uint64		kept = 0;
		int			base = word * 64;

		/* A whole word without a branch a row; a partial one row by row. */
		if (selected == UINT64_MAX)
		{
			for (int bit = 0; bit < 64; bit++)
				kept |= (uint64) ((!column.isnull[base + bit]) &
								  (DatumGetBool(column.values[base + bit]) ? 1 : 0)) << bit;
		}
		else
		{
			while (selected != 0)
			{
				int			bit = pg_rightmost_one_pos64(selected);

				row = base + bit;
				if (!column.isnull[row] && DatumGetBool(column.values[row]))
					kept |= UINT64CONST(1) << bit;
				selected &= selected - 1;
			}
		}
		state->filter_bits[word] = kept;
	}
	return (TessRowMask) {rows->nrows, state->filter_bits};
}

/*
 * Add one batch to the aggregate: its partial through the batch function,
 * or, for a batch with few survivors, their values gathered into a column
 * of the aggregate's own, since a call costs more than the rows it would
 * sum; the column is evaluated when it fills or the input ends. The batch
 * is the projection's wrapper, which computes the argument's column.
 */
static void
accumulate(TessAggState *state, AggValue *value, TessBatch *batch, int nrows)
{
	TessDatumColumn computed = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	const TessDatumColumn *column = &computed;
	TessRowMask rows = batch->rows;
	int			row = -1;

	if (value->filter >= 0)
	{
		rows = agg_filtered_rows(state, batch, value->filter, &batch->rows);
		nrows = tess_row_mask_count(&rows);
	}
	if (value->computed < 0)
	{
		evaluate(state, value, NULL, &rows);
		return;
	}
	/* A numeric aggregate of its own reads its argument's decimals. */
	computed.accept_decimals = fast_decimals(value);
	batch->ops->get_datum_column(batch,
								 state->child_layout.ncolumns + value->computed,
								 &rows, TESS_COLUMN_FOR_PROJECTION, &computed);
	if (computed.values == NULL || computed.isnull == NULL ||
		computed.nrows != batch->rows.nrows)
		elog(ERROR, "Tessera projection returned an invalid column");
	/*
	 * Above a gather: a row a participant's partial value, NULL skipped (a
	 * participant without rows gives a count of 0 and NULL otherwise), not
	 * the rows a batch function would count.
	 */
	if (state->finalize)
	{
		while ((row = tess_row_mask_next(&rows, row)) >= 0)
		{
#ifdef HAVE_INT128
			if (value->kind == AGG_GENERIC && value->generic->fast != FAST_NONE)
				agg_fast_merge(state, value->generic, computed.values[row],
						   computed.isnull[row]);
			else
#endif
			if (value->kind == AGG_GENERIC)
				agg_generic_combine(state, value->generic, computed.values[row],
								computed.isnull[row]);
			else if (!computed.isnull[row])
				join_partial(value, computed.values[row]);
		}
		return;
	}
	if (value->generic != NULL)
	{
		value->generic->columns[0] = computed;
		for (int arg = 1; arg < value->generic->nargs; arg++)
		{
			TessDatumColumn *other = &value->generic->columns[arg];

			*other = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
			batch->ops->get_datum_column(batch,
										 state->child_layout.ncolumns + value->computed + arg,
										 &rows, TESS_COLUMN_FOR_PROJECTION, other);
			if (other->values == NULL || other->isnull == NULL ||
				other->nrows != batch->rows.nrows)
				elog(ERROR, "Tessera projection returned an invalid column");
		}
		if (value->distinct != NULL)
		{
			TessRowMask pairs = agg_distinct_rows(state, value, rows.nrows, NULL, &rows, column);

			agg_generic_accumulate(state, value->generic, &pairs);
		}
		else
			agg_generic_accumulate(state, value->generic, &rows);
		return;
	}
	if (value->distinct != NULL)
	{
		TessRowMask pairs = agg_distinct_rows(state, value, rows.nrows, NULL, &rows, column);

		evaluate(state, value, column, &pairs);
		return;
	}
	if (nrows > AGG_GATHER_ROWS)
	{
		evaluate(state, value, column, &rows);
		return;
	}
	while ((row = tess_row_mask_next(&rows, row)) >= 0)
	{
		if (value->ngathered == 64)
			flush_gathered(state, value);
		value->gathered_values[value->ngathered] = column->values[row];
		value->gathered_isnull[value->ngathered] = column->isnull[row];
		value->ngathered++;
	}
}

/* Empty every distinct set, before the input is read. */
void
agg_reset_distinct(TessAggState *state)
{
	for (int index = 0; index < state->nvalues; index++)
		if (state->values[index].distinct != NULL)
			agg_distinct_reset(state, &state->values[index]);
}

/* Read every batch of the child into the running values. */
static void
drain(TessAggState *state)
{
	agg_reset_distinct(state);
	/* The states of a previous scan go, with the callbacks they registered. */
	if (state->generic_agg != NULL)
		ReScanExprContext(state->generic_agg->curaggcontext);
	for (int index = 0; index < state->nvalues; index++)
		if (state->values[index].generic != NULL)
			agg_generic_reset(state, state->values[index].generic);
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->input);
		int			rows;

		if (batch == NULL)
			break;
		rows = tess_row_mask_count(&batch->rows);
		state->batches++;
		state->rows += rows;
		if (rows > 0)
		{
			TessBatch  *input = batch;

			ResetExprContext(state->css.ss.ps.ps_ExprContext);
			if (state->projection != NULL)
				input = tess_projection_wrap(state->projection, batch);
			agg_read_in_order(state, input);
			for (int index = 0; index < state->nvalues; index++)
				accumulate(state, &state->values[index], input, rows);
			if (state->projection != NULL)
				input->ops->release(input);
		}
		tess_input_finish(state->input);
	}
	for (int index = 0; index < state->nvalues; index++)
		flush_gathered(state, &state->values[index]);
}

/* The one result row: the aggregates in the scan slot. */
static TupleTableSlot *
result_row(TessAggState *state)
{
	TupleTableSlot *scan = state->css.ss.ss_ScanTupleSlot;

	ExecClearTuple(scan);
	for (int index = 0; index < state->nvalues; index++)
	{
		AggValue   *value = &state->values[index];

		scan->tts_isnull[index] = !value->has_value;
		switch (value->kind)
		{
			case AGG_COUNT:
				scan->tts_values[index] = Int64GetDatum(value->total);
				scan->tts_isnull[index] = false;
				break;
			case AGG_SUM:
				scan->tts_values[index] = Int64GetDatum(value->total);
				break;
			case AGG_MIN:
			case AGG_MAX:
				scan->tts_values[index] = value->wide ?
					Int64GetDatum(value->extreme) :
					Int32GetDatum((int32) value->extreme);
				break;
			case AGG_GENERIC:
				scan->tts_values[index] = agg_generic_value(value->generic,
														&scan->tts_isnull[index]);
				break;
		}
	}
	return ExecStoreVirtualTuple(scan);
}

/*
 * A column of the groups' own batch: a key's values as the walk gathered
 * them (a number already its value, which lives in the dictionary until
 * the next partition's rows are read, after the batch is done with), or an
 * aggregate's values.
 */
static void
groups_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessAggState *state = (TessAggState *) batch->private_data;

	if (column < 0 || column >= state->nkeys + state->nvalues)
		elog(ERROR, "TessAgg has no column %d", column);
	if (column < state->nkeys)
	{
		result->values = state->key_values[column];
		result->isnull = state->key_isnull[column];
	}
	else
	{
		result->values = &state->agg_values[(column - state->nkeys) * AGG_GROUP_ROWS];
		result->isnull = &state->agg_isnull[(column - state->nkeys) * AGG_GROUP_ROWS];
	}
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps groups_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = groups_get_column,
};

/*
 * GROUP BY: after the input, a batch of groups per call to a batch-aware
 * parent, or their rows one by one to a row-wise parent.
 */
static TupleTableSlot *
group_exec(TessAggState *state)
{
	bool		rows = tess_output_request(state->output)->output_mode ==
		TESS_OUTPUT_ROWS;

	if (!state->drained)
		agg_group_drain(state);
	if (rows && state->published != NULL)
	{
		state->next_row = tess_row_mask_next(&state->published->rows,
											 state->next_row);
		if (state->next_row >= 0)
			return tess_output_select(state->output, state->next_row);
		tess_output_finish(state->output);
	}
	tess_output_release(state->output);
	state->published = agg_next_groups(state);
	if (state->published == NULL)
	{
		state->done = true;
		return NULL;
	}
	state->next_row = tess_row_mask_next(&state->published->rows, -1);
	return tess_output_publish(state->output, state->published);
}

/*
 * The result row, once: the aggregates in the scan slot, HAVING over
 * them, and the plan's projection when the targets are not the bare
 * aggregates, as the executor set it up for the scan tuple.
 */
static TupleTableSlot *
agg_exec(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;
	ExprContext *econtext = css->ss.ps.ps_ExprContext;
	TupleTableSlot *row;
	TessBatch  *batch;

	if (state->nkeys > 0)
		return state->done ? NULL : group_exec(state);
	if (state->done)
	{
		/* Served to a row-wise parent, or read by a batch-aware one. */
		tess_output_finish(state->output);
		tess_output_release(state->output);
		return NULL;
	}
	drain(state);
	state->done = true;
	ResetExprContext(econtext);
	econtext->ecxt_scantuple = result_row(state);
	if (css->ss.ps.qual != NULL && !ExecQual(css->ss.ps.qual, econtext))
		return NULL;
	row = css->ss.ps.ps_ProjInfo != NULL ? ExecProject(css->ss.ps.ps_ProjInfo) :
		ExecCopySlot(css->ss.ps.ps_ResultTupleSlot, econtext->ecxt_scantuple);
	tess_builder_reset(state->builder);
	tess_builder_append_slot(state->builder, row);
	batch = tess_builder_finish(state->builder, InvalidOid);
	return tess_output_publish(state->output, batch);
}

static void
agg_end(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	if (state->setop >= 0)
	{
		ExecEndNode(state->sides[0]);
		ExecEndNode(state->sides[1]);
	}
	else
		ExecEndNode(state->child);
	agg_spill_free(state);
	rows_spill_free(state);
	if (state->table_context != NULL)
		MemoryContextDelete(state->table_context);
}

static void
agg_rescan(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;

	tess_output_clear(state->output);
	/* INTERSECT or EXCEPT: both sides again, from the left. */
	if (state->setop >= 0)
	{
		for (int side = 0; side < 2; side++)
		{
			tess_projection_reset(state->side_projections[side]);
			tess_rescan_child(&css->ss.ps, state->sides[side], state->side_inputs[side]);
		}
		agg_setop_side(state, 0);
		state->setop_count = 0;
		state->setop_group = 0;
		state->setop_copies = -1;
	}
	else
	{
		if (state->projection != NULL)
			tess_projection_reset(state->projection);
		tess_rescan_child(&css->ss.ps, state->child, state->input);
	}
	for (int index = 0; index < state->nvalues; index++)
	{
		state->values[index].total = 0;
		state->values[index].has_value = false;
		state->values[index].ngathered = 0;
	}
	state->done = false;
	/* GROUP BY: the table is built again from the rescanned child. */
	agg_spill_free(state);
	rows_spill_free(state);
	state->drained = false;
	state->input_done = false;
	state->published = NULL;
	/*
	 * EXPLAIN's counters total every scan, as the core's instrumentation
	 * does; the early emits count this scan's rows from here.
	 */
	state->emit_rows = state->rows;
	state->partial_spill = false;
}

/* This participant's counters. */
static void
agg_counters(TessAggState *state, uint64 *values)
{
	memset(values, 0, AGG_NCOUNTERS * sizeof(uint64));
	values[AGG_BATCHES] = state->batches;
	values[AGG_ROWS] = state->rows;
	values[AGG_CALLS] = state->calls;
	if (state->setop >= 0)
		for (int side = 0; side < 2; side++)
		{
			const TessProjectionStats *computed =
				tess_projection_stats(state->side_projections[side]);

			values[AGG_COMPUTED] += computed->chain_datums + computed->row_datums;
		}
	else if (state->projection != NULL)
	{
		const TessProjectionStats *computed = tess_projection_stats(state->projection);

		values[AGG_COMPUTED] = computed->chain_datums + computed->row_datums;
	}
	values[AGG_GROUPS] = state->groups;
	values[AGG_MEMORY] = state->peak_memory;
	values[AGG_GROWS] = state->grows;
	values[AGG_PARTITIONS] = state->partitions;
	values[AGG_EVICTIONS] = state->evictions;
	values[AGG_SPILLED] = state->spilled;
	values[AGG_DISK] = state->disk_bytes;
	values[AGG_SPLITS] = state->splits;
	values[AGG_EARLY] = state->early_emits;
	values[AGG_SPILLED_ROWS] = state->spilled_rows;
}

/* The totals of every participant in a parallel plan, else the node's own. */
static void
agg_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessAggState *state = (TessAggState *) css;
	const uint64 *totals;
	uint64		own[AGG_NCOUNTERS];

	if (state->nkeys > 0)
	{
		CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
		List	   *context = set_deparse_context_plan(es->deparse_cxt,
													   css->ss.ps.plan, ancestors);
		bool		useprefix = es->rtable_size > 1 || es->verbose;
		List	   *keys = NIL;

		foreach_node(TargetEntry, entry, cscan->custom_scan_tlist)
		{
			if (foreach_current_index(entry) < state->nkeys)
				keys = lappend(keys, deparse_expression((Node *) entry->expr,
														context, useprefix,
														false));
		}
		ExplainPropertyList("Group Key", keys, es);
		if (state->setop >= 0)
			ExplainPropertyText("Set Operation",
								state->setop == SETOPCMD_INTERSECT ? "Intersect" :
								state->setop == SETOPCMD_INTERSECT_ALL ? "Intersect All" :
								state->setop == SETOPCMD_EXCEPT ? "Except" : "Except All", es);
	}
	if (state->partial || state->finalize)
		ExplainPropertyText("Partial Mode", state->partial ? "Partial" : "Finalize", es);
	if (!es->analyze)
		return;
	agg_counters(state, own);
	totals = tess_shared_stats_totals_or(state->stats, own);
	/* As the core's hashed aggregate shows its table and its spill. */
	if (state->nkeys > 0)
	{
		tess_explain_kb("Memory Usage", totals[AGG_MEMORY], es);
		if (totals[AGG_PARTITIONS] > 0)
		{
			ExplainPropertyInteger("Batches", NULL, totals[AGG_PARTITIONS], es);
			tess_explain_kb("Disk Usage", totals[AGG_DISK], es);
		}
	}
	/* The batches, the kernels and the table's work: VERBOSE only. */
	if (!es->verbose)
		return;
	ExplainPropertyInteger("Input Batches", NULL, totals[AGG_BATCHES], es);
	ExplainPropertyInteger("Input Rows", NULL, totals[AGG_ROWS], es);
	ExplainPropertyInteger("Kernel Calls", NULL, totals[AGG_CALLS], es);
	if (state->projection != NULL)
		ExplainPropertyInteger("Computed Datums", NULL, totals[AGG_COMPUTED], es);
	if (state->nkeys > 0)
	{
		ExplainPropertyInteger("Groups", NULL, totals[AGG_GROUPS], es);
		ExplainPropertyInteger("Table Grows", NULL, totals[AGG_GROWS], es);
		if (totals[AGG_EARLY] > 0)
			ExplainPropertyInteger("Early Emits", NULL, totals[AGG_EARLY], es);
		if (totals[AGG_PARTITIONS] > 0)
		{
			ExplainPropertyInteger("Evictions", NULL, totals[AGG_EVICTIONS], es);
			ExplainPropertyInteger("Spilled Chunks", NULL, totals[AGG_SPILLED], es);
			if (totals[AGG_SPILLED_ROWS] > 0)
				ExplainPropertyInteger("Spilled Rows", NULL, totals[AGG_SPILLED_ROWS], es);
			if (totals[AGG_SPLITS] > 0)
				ExplainPropertyInteger("Split Partitions", NULL, totals[AGG_SPLITS], es);
		}
	}
}

/*
 * A parallel plan: the node shares only its counters, in the rows of its
 * chunk; the child divides the work and the Finalize Aggregate above the
 * Gather combines the participants' values.
 */
TESS_NODE_STATS_CALLBACKS(agg, TessAggState, AGG_NCOUNTERS, agg_counters)

static const CustomExecMethods agg_exec_methods = {
	.CustomName = "TessAgg",
	.BeginCustomScan = agg_begin,
	.ExecCustomScan = agg_exec,
	.EndCustomScan = agg_end,
	.ReScanCustomScan = agg_rescan,
	.ExplainCustomScan = agg_explain,
	TESS_NODE_STATS_METHODS(agg),
};

static Node *
agg_create_state(CustomScan *cscan)
{
	TessAggState *state = (TessAggState *)
		newNode(sizeof(TessAggState), T_CustomScanState);

	state->css.methods = &agg_exec_methods;
	return (Node *) state;
}

const CustomScanMethods tess_agg_scan_methods = {
	.CustomName = "TessAgg",
	.CreateCustomScanState = agg_create_state,
};

const TessNode tess_agg_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_AGG_NODE_NAME,
};

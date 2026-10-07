/*
 * TessHashJoin's callbacks: the node set up from its plan, ended,
 * rescanned and explained, and the shared memory of a parallel plan; the
 * node's methods. The join itself is in hashjoin.c and hashjoin_pairs.c.
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

static const CustomExecMethods join_exec_methods;

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

	tess_node_require_forward(eflags, "TessHashJoin");
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
	join_reset_values(state);
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
		/* Computed columns follow the scan tuple's, as for TessFilter. */
		state->projection = tess_node_projection(css, css->ss.ss_ScanTupleSlot,
												 &state->scan_layout, state->ncolumns,
												 info.computed);
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

static void
join_end(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	ExecEndNode(state->outer);
	ExecEndNode(state->inner);
	join_spill_free(state);
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
		join_forget_marks(state);
	for (int chunk = 0; chunk < state->mark_chunks; chunk++)
		memset(state->marks[chunk], 0,
			   sizeof(uint64) * join_marks_of(state, state->table.chunk_lens[chunk]));
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
		join_round_leave(state);
	if (state->parallel.shared != NULL)
		join_leave_shared(state, false);
	/*
	 * A table that spilled is no longer whole: the inner child is read
	 * again, rescanned here when no parameter of it changed.
	 */
	if (state->spill != NULL)
	{
		join_spill_free(state);
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
	Size		limit = tess_hash_memory_limit();

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
 * totals of every participant in a parallel plan. The memory is the most
 * the tables, the copies of inner values and spilling took, and Overrun
 * what of it exceeded hash_mem, which happens only where a partition on
 * disk larger than hash_mem is joined whole (docs/nodes.md).
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
	tess_explain_kb("Memory Usage", totals[JOIN_MEMORY], es);
	if (state->parallel.shared_budget > 0)
		overrun = totals[JOIN_MEMORY] > state->parallel.shared_budget ?
			totals[JOIN_MEMORY] - state->parallel.shared_budget : 0;
	else
		overrun = totals[JOIN_OVERRUN];
	if (overrun > 0)
		tess_explain_kb("Overrun", overrun, es);
	/* As the core's hash shows its batches on disk. */
	if (totals[JOIN_BATCHES] > 0)
	{
		ExplainPropertyInteger("Batches", NULL, totals[JOIN_BATCHES], es);
		tess_explain_kb("Disk Usage", totals[JOIN_DISK], es);
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
 * A parallel plan: the outer child divides the rows. Without a shared
 * table every participant builds the whole inner side into a table of its
 * own, as the core's hash join without a shared table does, and the chunk
 * holds only the counters, in the rows of the participants; with one
 * (Shared Table), the chunk holds the shared build's state before them,
 * and the participants build one table together (hashjoin_shared.c).
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
	dsa_area   *area = join_query_dsa(state);
	Size		budget = tess_hash_memory_limit();

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
		tess_spill_shared_reset(&state->parallel.shared->fileset);
	if (budget > SIZE_MAX / Max(state->parallel.shared->participants, 1))
		budget = SIZE_MAX / Max(state->parallel.shared->participants, 1);
	/* One table holds every participant's chunks: its limit bounds them. */
	state->parallel.shared_budget =
		tess_table_memory_limit(budget * state->parallel.shared->participants);
	check(state, state->kernels->table_spill_init(dsa_get_address(area,
																  state->parallel.shared->spill_words),
												  state->parallel.shared->spill_nwords, true,
												  (uint64) state->parallel.shared_budget,
												  &state->status));
	for (int list = 0; list < 2 * state->parallel.shared->participants; list++)
		*join_participant_list(state, list / 2, list % 2 == 1) = InvalidDsaPointer;
	for (int partition = 0; partition < JOIN_SPILL_MAX_PARTITIONS; partition++)
	{
		pg_atomic_init_u64(&join_part_stats(state, partition)[0], 0);
		pg_atomic_init_u64(&join_part_stats(state, partition)[1], 0);
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
			join_round_leave(state);
		join_leave_shared(state, false);
		/* Its files go with the set's. */
		join_spill_free(state);
		join_free_shared_table(state);
		join_free_rounds(state);
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
		state->parallel.shared_budget =
			tess_table_memory_limit(Min(tess_hash_memory_limit(),
										SIZE_MAX / Max(state->parallel.shared->participants, 1)) *
									state->parallel.shared->participants);
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
		join_round_leave(state);
	if (state->parallel.shared != NULL)
		join_leave_shared(state, false);
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

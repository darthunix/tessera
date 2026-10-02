#include "postgres.h"

#include "access/htup_details.h"
#include "access/parallel.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/execPartition.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/restrictinfo.h"
#include "partitioning/partprune.h"
#include "storage/lwlock.h"

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessAppend stands in for the core's Append under a batch parent
 * (docs/nodes.md): it reads its batch children in turn and gives its
 * parent each child's batches as they are, through a batch of its own
 * whose columns are the child's, renumbered by the child's layout. Under
 * the core's Append each child's batches became rows, which a pack node
 * made batches again: 13 ms of an aggregate's 31 over two filtered scans
 * of a million rows each. The node's path is built only where a batch
 * parent asks for a batch child over an Append (tess_batch_input_path),
 * so that a row-wise parent keeps the core's Append. In a parallel plan
 * the node shares the children out as the core's Parallel Append does: a
 * child that is not partial goes to one participant, a partial one to any
 * that comes while it has work, each dividing its pages with the others.
 * The node prunes the partitions while executing as the core's Append
 * does, through the core's pruning state: at its start by the query's
 * parameters and stable functions, then by the parameters of execution.
 */

/* The core's cost of a row through an Append (costsize.c), which the node saves. */
#define APPEND_CPU_COST_MULTIPLIER 0.5

/* The node's counter summed over the participants: the batches given out. */
#define APPEND_NCOUNTERS 1

/*
 * The children shared out in a parallel plan, after the counters in the
 * node's chunk, as the core's ParallelAppendState: the child a worker
 * looks at first, and the children that need no more participants.
 */
typedef struct AppendShared
{
	LWLock		lock;
	int			next_plan;
	bool		finished[FLEXIBLE_ARRAY_MEMBER];
} AppendShared;

typedef struct TessAppendState
{
	CustomScanState css;
	/*
	 * The children the plan has, and of them those the initial pruning
	 * left, which the node initialized and reads.
	 */
	int			nplanned;
	int			nchildren;
	PlanState **children;
	TessInput **inputs;
	/* Each child's layout: the column of each of its targets. */
	TessLayout *layouts;
	int			ncolumns;
	/* The children before this one are not partial. */
	int			first_partial;
	/*
	 * Pruning while executing: the core's state, NULL without it; with
	 * pruning by the parameters of execution, the children valid for the
	 * current ones, once known.
	 */
	PartitionPruneState *prune;
	bool		exec_prune;
	bool		valid_known;
	Bitmapset  *valid;
	/*
	 * Pruning by a hash join's keys (TessHashJoin): each planned child's
	 * number here, -1 for one the initial pruning removed; the core's states
	 * of the join's descriptions over the planned children, by the join's
	 * parameters (a key, the lowest, the highest); once the join set them,
	 * the children its keys left and how many they removed.
	 */
	int		   *planned_child;
	PartitionPruneState *join_values;
	PartitionPruneState *join_range;
	int			join_params[3];
	bool		join_set;
	Bitmapset  *join_valid;
	int			join_removed;
	/* The child being read, -1 before the first, and its batch given out. */
	int			current;
	bool		done;
	TessBatch  *child_batch;
	/* In a parallel plan: the children shared out and the counters. */
	AppendShared *shared;
	TessSharedStats *stats;
	TessOutput *output;
	bool		requested;
	/* The batch given out: the child's rows, its columns renumbered. */
	TessBatch	batch;
	bool		published;
	int			served;
	uint64		batches;
} TessAppendState;

static const CustomExecMethods append_exec_methods;

static Plan *append_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
						 List *tlist, List *clauses, List *custom_plans);

static const CustomPathMethods append_path_methods = {
	.CustomName = "TessAppend",
	.PlanCustomPath = append_plan,
};

/* ---------------------------------------------------------------- planning */

/*
 * Whether a batch child only packs the rows of a core path: a pack over a
 * subquery planned as a batch path forwards its batches.
 */
static bool
packs_rows(Path *child)
{
	return tess_path_node(child) == &tess_pack_node && !tess_pack_forwards(child);
}

/*
 * Whether rel is a set operation's, whose targets are its output columns:
 * Vars of no relation.
 */
static bool
setop_relation(RelOptInfo *rel, PathTarget *target)
{
	if (rel->reloptkind != RELOPT_UPPER_REL)
		return false;
	foreach_ptr(Node, expr, target->exprs)
	{
		if (!IsA(expr, Var) || ((Var *) expr)->varno != 0)
			return false;
	}
	return true;
}

/*
 * The node's path in place of an Append of a base relation's children: a
 * partitioned table, an inheritance tree, a UNION ALL the planner made a
 * relation of; or of a set operation's children, the branches of a UNION
 * the planner did not make a relation of. Serial, partial, or
 * parallel-aware as a Parallel Append, whose children before
 * first_partial_path are not partial. When every child has a batch path
 * and one of them at least does more than pack rows; NULL otherwise.
 */
static CustomPath *
append_wrap(PlannerInfo *root, Path *path)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	AppendPath *append = (AppendPath *) path;
	RelOptInfo *rel = path->parent;
	List	   *children = NIL;
	bool		batches = false;
	CustomPath *built;
	Cost		saved;

	if (!*tess_runtime_api()->settings->enable || !IsA(path, AppendPath) ||
		rel == NULL || path->pathtarget == NULL ||
		!(IS_SIMPLE_REL(rel) || setop_relation(rel, path->pathtarget)) ||
		root->parse->commandType != CMD_SELECT ||
		root->parse->rowMarks != NIL || path->param_info != NULL ||
		list_length(append->subpaths) < 2 ||
		list_length(path->pathtarget->exprs) > MaxTupleAttributeNumber)
		return NULL;
	/* A branch's constant column would stand for every row's. */
	if (!IS_SIMPLE_REL(rel) && tess_path_setop_constant(path))
		return NULL;
	foreach_ptr(Path, subpath, append->subpaths)
	{
		Path	   *child = tess_batch_input_path(root, subpath);

		if (child == NULL)
			return NULL;
		if (!packs_rows(child))
			batches = true;
		children = lappend(children, child);
	}
	if (!batches)
		return NULL;
	config.template_path = path;
	config.methods = &append_path_methods;
	config.node = &tess_append_node;
	config.children = children;
	config.node_data = (Node *) makeInteger(path->parallel_aware ?
											append->first_partial_path : 0);
	built = tess_path_create(&config);
	saved = APPEND_CPU_COST_MULTIPLIER * cpu_tuple_cost * path->rows;
	built->path.total_cost = Max(built->path.startup_cost, built->path.total_cost - saved);
	return built;
}

/*
 * The core's description of pruning the children while executing, made as
 * create_append_plan makes it, from the relation's clauses and the paths
 * of the children (whose parents are the partitions); NULL where the
 * clauses prune nothing then. The planner keeps such descriptions for
 * set_plan_references, which carries into the plan those of Append and
 * MergeAppend only: the node takes its own off the list, into its plan
 * data.
 */
static PartitionPruneInfo *
prune_info(PlannerInfo *root, RelOptInfo *rel, List *children)
{
	List	   *clauses;
	int			index;
	PartitionPruneInfo *info;

	if (!enable_partition_pruning || !IS_SIMPLE_REL(rel))
		return NULL;
	clauses = extract_actual_clauses(rel->baserestrictinfo, false);
	if (clauses == NIL)
		return NULL;
	index = make_partition_pruneinfo(root, rel, children, clauses);
	if (index < 0)
		return NULL;
	info = llast_node(PartitionPruneInfo, root->partPruneInfos);
	if (index != list_length(root->partPruneInfos) - 1 ||
		!bms_equal(info->relids, rel->relids))
		elog(ERROR, "TessAppend found another pruning description than its own");
	root->partPruneInfos = list_delete_last(root->partPruneInfos);
	return info;
}

/*
 * A column per target. The clauses are the parent relation's, which the
 * core translated to every child: the children evaluate them. A set
 * operation's columns become its first child's targets, as the core's
 * Append shows them (tess_plan_create).
 */
static Plan *
append_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path, List *tlist,
			List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanWriter *writer;

	tess_path_get_info(best_path, &info);
	for (int index = 0; index < list_length(custom_plans); index++)
	{
		TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);

		if (!tess_plan_child(best_path, custom_plans, index, &child))
			elog(ERROR, "TessAppend expected batch children");
		if (child.layout.ntargets != list_length(tlist))
			elog(ERROR, "TessAppend child %d has %d targets, not %d", index,
				 child.layout.ntargets, list_length(tlist));
	}
	writer = tess_plan_writer_create(TESS_APPEND_DATA, TESS_APPEND_DATA_VERSION);
	tess_plan_write_int(writer, "children", list_length(custom_plans));
	tess_plan_write_int(writer, "first_partial", intVal(info.node_data));
	tess_plan_write_node(writer, "prune",
						 (Node *) prune_info(root, rel, best_path->custom_paths));
	config.methods = &tess_append_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

/* ---------------------------------------------------------------- execution */

static Node *
append_create_state(CustomScan *cscan)
{
	TessAppendState *state = (TessAppendState *) newNode(sizeof(TessAppendState),
														 T_CustomScanState);

	state->css.methods = &append_exec_methods;
	return (Node *) state;
}

/* A column of the batch given out: the child's column of that target. */
static void
append_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessAppendState *state = (TessAppendState *) batch->private_data;
	TessBatch  *child = state->child_batch;

	if (column < 0 || column >= state->ncolumns || child == NULL)
		elog(ERROR, "TessAppend has no column %d", column);
	child->ops->get_datum_column(child,
								 tess_layout_column(&state->layouts[state->current], column),
								 rows, purpose, result);
}

static const TessBatchOps append_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = append_get_column,
};

/*
 * The core's pruning state over the plan's description, whose range table
 * numbers are the node's query's own: set_plan_references added the
 * offset of the query's range table in the statement's to the node's
 * custom_relids, the relation's (create_customscan_plan), not to the plan
 * data, so a copy gets it here. The core makes the states of the
 * statement's descriptions only (ExecDoInitialPruning, before the nodes
 * start) and hands a node its own by number (ExecInitPartitionExecPruning):
 * the EState's lists hold this one alone for the two calls. The initial
 * pruning is done: valid receives the children to initialize. The core
 * exports no other way to make a pruning state, and a CustomScan's
 * description never reaches the statement's list (setrefs registers an
 * Append's alone). What the second call leaves besides the state: the
 * leaf partitions it kept join es_unpruned_relids, as an Append's do.
 */
static PartitionPruneState *
prune_start(TessAppendState *state, const PartitionPruneInfo *planned, Bitmapset **valid)
{
	EState	   *estate = state->css.ss.ps.state;
	CustomScan *cscan = castNode(CustomScan, state->css.ss.ps.plan);
	PartitionPruneInfo *info = copyObject(planned);
	int			offset = bms_next_member(cscan->custom_relids, -1) -
		bms_next_member(info->relids, -1);
	Bitmapset  *relids = NULL;
	int			member = -1;
	List	   *infos = estate->es_part_prune_infos;
	List	   *states = estate->es_part_prune_states;
	List	   *results = estate->es_part_prune_results;
	PartitionPruneState *prune;

	while ((member = bms_next_member(info->relids, member)) >= 0)
		relids = bms_add_member(relids, member + offset);
	if (!bms_equal(relids, cscan->custom_relids))
		elog(ERROR, "TessAppend's pruning description is not its relation's");
	info->relids = relids;
	foreach_node(List, hierarchy, info->prune_infos)
	{
		foreach_node(PartitionedRelPruneInfo, rel, hierarchy)
		{
			rel->rtindex += offset;
			for (int part = 0; part < rel->nparts; part++)
			{
				if (rel->leafpart_rti_map[part] != 0)
					rel->leafpart_rti_map[part] += offset;
			}
		}
	}
	estate->es_part_prune_infos = list_make1(info);
	estate->es_part_prune_states = NIL;
	estate->es_part_prune_results = NIL;
	PG_TRY();
	{
		ExecDoInitialPruning(estate);
		prune = ExecInitPartitionExecPruning(&state->css.ss.ps, state->nplanned, 0,
											 info->relids, valid);
	}
	PG_FINALLY();
	{
		estate->es_part_prune_infos = infos;
		estate->es_part_prune_states = states;
		estate->es_part_prune_results = results;
	}
	PG_END_TRY();
	return prune;
}

static void
append_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessAppendState *state = (TessAppendState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessPlanReader *reader;
	PartitionPruneInfo *planned;
	Bitmapset  *valid;
	int			first_partial;
	int			index = 0;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessAppend supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_append_node)
		elog(ERROR, "TessAppend received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, TESS_APPEND_DATA,
									 TESS_APPEND_DATA_VERSION);
	state->nplanned = tess_plan_read_int(reader, "children");
	first_partial = tess_plan_read_int(reader, "first_partial");
	planned = (PartitionPruneInfo *) tess_plan_read_node(reader, "prune");
	tess_plan_reader_finish(reader);
	state->current = -1;
	if (state->nplanned != info.nchildren || state->nplanned < 1 ||
		first_partial < 0 || first_partial > state->nplanned ||
		(planned != NULL && !IsA(planned, PartitionPruneInfo)))
		elog(ERROR, "TessAppend received a foreign plan");
	/*
	 * As the core's Append: the children the initial pruning left, none
	 * possibly, the first partial of them, and whether the parameters of
	 * execution prune further.
	 */
	valid = bms_add_range(NULL, 0, state->nplanned - 1);
	if (planned != NULL)
	{
		state->prune = prune_start(state, planned, &valid);
		state->exec_prune = state->prune->do_exec_prune;
	}
	state->nchildren = bms_num_members(valid);
	state->first_partial = state->nchildren;
	state->ncolumns = css->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;
	state->children = palloc_array(PlanState *, Max(state->nchildren, 1));
	state->inputs = palloc_array(TessInput *, Max(state->nchildren, 1));
	state->layouts = palloc_array(TessLayout, Max(state->nchildren, 1));
	state->planned_child = palloc_array(int, state->nplanned);
	foreach_ptr(Plan, plan, cscan->custom_plans)
	{
		int			planned_index = foreach_current_index(plan);

		if (info.child_names[planned_index] == NULL)
			elog(ERROR, "TessAppend expected batch children");
		state->planned_child[planned_index] = -1;
		if (!bms_is_member(planned_index, valid))
			continue;
		state->planned_child[planned_index] = index;
		if (planned_index >= first_partial && index < state->first_partial)
			state->first_partial = index;
		state->children[index] = ExecInitNode(plan, estate, eflags);
		css->custom_ps = lappend(css->custom_ps, state->children[index]);
		state->inputs[index] = tess_input_create(estate->es_query_cxt,
												 state->children[index]);
		state->layouts[index] = *tess_input_layout(state->inputs[index]);
		if (state->layouts[index].ntargets != state->ncolumns)
			elog(ERROR, "TessAppend child %d has %d targets, not %d", index,
				 state->layouts[index].ntargets, state->ncolumns);
		index++;
	}
	state->batch.abi_version = TESS_BATCH_ABI_VERSION;
	state->batch.struct_size = sizeof(TessBatch);
	state->batch.ops = &append_batch_ops;
	state->batch.private_data = state;
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot, &info.layout);
}

/* The children's columns of the targets in columns. */
static Bitmapset *
child_columns(const TessLayout *layout, const Bitmapset *columns)
{
	Bitmapset  *result = NULL;
	int			column = -1;

	while ((column = bms_next_member(columns, column)) >= 0)
		result = bms_add_member(result, tess_layout_column(layout, column));
	return result;
}

/*
 * Every child gets the parent's request in its own columns; a row-wise
 * parent is served every column, as the output helper reads the targets.
 */
static void
send_requests(TessAppendState *state)
{
	const TessRequest *request = tess_output_request(state->output);
	Bitmapset  *all = NULL;

	if (request->output_mode == TESS_OUTPUT_ROWS)
		all = bms_add_range(NULL, 0, state->ncolumns - 1);
	for (int index = 0; index < state->nchildren; index++)
	{
		TessRequest child = TESS_STRUCT_INITIALIZER(TessRequest);
		const TessLayout *layout = &state->layouts[index];

		if (all != NULL)
			child.projection_columns = child_columns(layout, all);
		else
		{
			child.filter_columns = child_columns(layout, request->filter_columns);
			child.projection_columns = child_columns(layout, request->projection_columns);
		}
		child.output_mode = TESS_OUTPUT_BATCH;
		child.max_batch_rows = request->max_batch_rows;
		tess_input_set_request(state->inputs[index], &child);
	}
	state->requested = true;
}

/*
 * The children valid for the parameters of execution, once per set of
 * them (ExecFindMatchingSubPlans, numbered among the children the node
 * initialized); in a parallel plan the others are finished for every
 * participant, the lock held, as the core's
 * mark_invalid_subplans_as_finished does.
 */
static void
find_valid(TessAppendState *state)
{
	if ((!state->exec_prune && !state->join_set) || state->valid_known)
		return;
	state->valid = state->exec_prune ? ExecFindMatchingSubPlans(state->prune, false, NULL) :
		bms_add_range(NULL, 0, state->nchildren - 1);
	if (state->join_set)
		state->valid = bms_int_members(state->valid, state->join_valid);
	state->valid_known = true;
	if (state->shared != NULL)
	{
		for (int index = 0; index < state->nchildren; index++)
		{
			if (!bms_is_member(index, state->valid))
				state->shared->finished[index] = true;
		}
	}
}

/*
 * The leader's next child in a parallel plan, as the core's
 * choose_next_subplan_for_leader: from the last child down, so that the
 * workers, which start from the first, take the costly children that are
 * not partial and the leader the partial ones, and can stop gathering
 * early. A child that is not partial is finished once chosen.
 */
static bool
choose_for_leader(TessAppendState *state)
{
	AppendShared *shared = state->shared;

	LWLockAcquire(&shared->lock, LW_EXCLUSIVE);
	if (state->current >= 0)
		shared->finished[state->current] = true;
	else
	{
		state->current = state->nchildren - 1;
		find_valid(state);
	}
	while (shared->finished[state->current])
	{
		if (state->current == 0)
		{
			shared->next_plan = -1;
			state->current = -1;
			LWLockRelease(&shared->lock);
			return false;
		}
		state->current--;
	}
	if (state->current < state->first_partial)
		shared->finished[state->current] = true;
	LWLockRelease(&shared->lock);
	return true;
}

/* The valid child after index, or -1. */
static int
next_child(TessAppendState *state, int index)
{
	if (state->exec_prune || state->join_set)
		return bms_next_member(state->valid, index);
	return index + 1 < state->nchildren ? index + 1 : -1;
}

/*
 * A worker's next child in a parallel plan, as the core's
 * choose_next_subplan_for_worker: the first that is not finished from
 * next_plan on, going round to the first partial child; next_plan moves
 * past it.
 */
static bool
choose_for_worker(TessAppendState *state)
{
	AppendShared *shared = state->shared;
	int			start;

	LWLockAcquire(&shared->lock, LW_EXCLUSIVE);
	if (state->current >= 0)
		shared->finished[state->current] = true;
	else
		find_valid(state);
	if (shared->next_plan < 0)
	{
		LWLockRelease(&shared->lock);
		return false;
	}
	start = shared->next_plan;
	while (shared->finished[shared->next_plan])
	{
		int			next = next_child(state, shared->next_plan);

		if (next >= 0)
			shared->next_plan = next;
		else if (start > state->first_partial)
		{
			next = next_child(state, state->first_partial - 1);
			shared->next_plan = next < 0 ? start : next;
		}
		else
			shared->next_plan = start;
		if (shared->next_plan == start)
		{
			shared->next_plan = -1;
			LWLockRelease(&shared->lock);
			return false;
		}
	}
	state->current = shared->next_plan;
	shared->next_plan = next_child(state, shared->next_plan);
	if (shared->next_plan < 0)
		shared->next_plan = next_child(state, state->first_partial - 1);
	if (state->current < state->first_partial)
		shared->finished[state->current] = true;
	LWLockRelease(&shared->lock);
	return true;
}

/* The next child to read, the children in turn without shared memory. */
static bool
choose_next(TessAppendState *state)
{
	if (state->shared != NULL)
		return IsParallelWorker() ? choose_for_worker(state) : choose_for_leader(state);
	find_valid(state);
	state->current = next_child(state, state->current);
	return state->current >= 0;
}

/* The next child batch with rows; false at the end. */
static bool
append_next(TessAppendState *state)
{
	/* The batch given out was consumed: its child may go on. */
	if (state->child_batch != NULL)
	{
		tess_input_finish(state->inputs[state->current]);
		state->child_batch = NULL;
	}
	/* Initial pruning may leave no child. */
	if (state->done || state->nchildren == 0)
		return false;
	if (state->current < 0 && !choose_next(state))
	{
		state->done = true;
		return false;
	}
	for (;;)
	{
		TessInput  *input = state->inputs[state->current];
		TessBatch  *batch = tess_input_next(input);

		if (batch == NULL)
		{
			if (!choose_next(state))
			{
				state->done = true;
				return false;
			}
			continue;
		}
		if (tess_row_mask_count(&batch->rows) == 0)
		{
			tess_input_finish(input);
			continue;
		}
		state->child_batch = batch;
		state->batch.rows = batch->rows;
		state->batch.table_oid = batch->table_oid;
		state->batches++;
		return true;
	}
}

static TupleTableSlot *
append_exec(CustomScanState *css)
{
	TessAppendState *state = (TessAppendState *) css;
	bool		rows;

	CHECK_FOR_INTERRUPTS();
	if (!state->requested)
		send_requests(state);
	rows = tess_output_request(state->output)->output_mode == TESS_OUTPUT_ROWS;
	if (rows && state->published)
	{
		int			next = tess_row_mask_next(&state->batch.rows, state->served);

		if (next >= 0)
		{
			state->served = next;
			return tess_output_select(state->output, next);
		}
		tess_output_finish(state->output);
	}
	tess_output_release(state->output);
	state->published = false;
	if (!append_next(state))
		return NULL;
	state->published = true;
	state->served = tess_row_mask_next(&state->batch.rows, -1);
	return tess_output_publish(state->output, &state->batch);
}

static void
append_end(CustomScanState *css)
{
	TessAppendState *state = (TessAppendState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	for (int index = 0; index < state->nchildren; index++)
		ExecEndNode(state->children[index]);
}

static void
append_rescan(CustomScanState *css)
{
	TessAppendState *state = (TessAppendState *) css;

	tess_output_clear(state->output);
	state->published = false;
	state->child_batch = NULL;
	/* Parameters of the pruning changed: the valid children are found anew. */
	if (state->exec_prune && bms_overlap(css->ss.ps.chgParam, state->prune->execparamids))
	{
		bms_free(state->valid);
		state->valid = NULL;
		state->valid_known = false;
	}
	/*
	 * The children a join's keys left stay until its next build sets them
	 * anew, which may come before the node's rescan: the core rescans a
	 * child at its first call after the parent's. They are intersected again.
	 */
	if (state->join_set)
	{
		bms_free(state->valid);
		state->valid = NULL;
		state->valid_known = false;
	}
	for (int index = 0; index < state->nchildren; index++)
	{
		/* The core passes changed parameters to outer and inner plans only. */
		if (css->ss.ps.chgParam != NULL)
			UpdateChangedParamSet(state->children[index], css->ss.ps.chgParam);
		ExecReScan(state->children[index]);
		tess_input_rescan(state->inputs[index]);
	}
	state->current = -1;
	state->done = false;
	/* The batches total every scan, as the core's instrumentation does. */
}

/*
 * The children the initial pruning removed; with ANALYZE, the batches
 * given out, every participant's in a parallel plan.
 */
static void
append_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessAppendState *state = (TessAppendState *) css;
	uint64		own = state->batches;
	const uint64 *totals;

	/* As the core's Append shows the children the initial pruning removed. */
	if (state->nchildren < state->nplanned)
		ExplainPropertyInteger("Subplans Removed", NULL,
							   state->nplanned - state->nchildren, es);
	if (!es->analyze)
		return;
	if (state->join_values != NULL)
		ExplainPropertyInteger("Subplans Removed by Join", NULL, state->join_removed, es);
	if (!es->verbose)
		return;
	totals = tess_shared_stats_totals_or(state->stats, &own);
	ExplainPropertyInteger("Batches", NULL, totals[0], es);
}

/*
 * A parallel plan: the node's chunk holds the counters' rows, then the
 * children shared out. The leader lays both out, a worker attaches.
 */
static Size
shared_size(TessAppendState *state)
{
	return MAXALIGN(offsetof(AppendShared, finished) + sizeof(bool) * state->nchildren);
}

static Size
append_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	TessAppendState *state = (TessAppendState *) css;

	return add_size(tess_shared_stats_estimate(APPEND_NCOUNTERS, pcxt->nworkers),
					shared_size(state));
}

static void
reset_shared(TessAppendState *state)
{
	state->shared->next_plan = 0;
	memset(state->shared->finished, 0, sizeof(bool) * state->nchildren);
}

static void
append_initialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	TessAppendState *state = (TessAppendState *) css;

	state->stats = tess_shared_stats_setup(state->stats, css->ss.ps.state->es_query_cxt,
										   coordinate, APPEND_NCOUNTERS, pcxt->nworkers,
										   pcxt->seg);
	state->shared = (AppendShared *) ((char *) coordinate +
									  tess_shared_stats_size(coordinate));
	/*
	 * The lock of a parallel append, which this is: its waits show as the
	 * core's ParallelAppend. A tranche of the node's own would be taken
	 * once a cluster, which a module loaded per session cannot do.
	 */
	LWLockInitialize(&state->shared->lock, LWTRANCHE_PARALLEL_APPEND);
	reset_shared(state);
}

static void
append_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	TessAppendState *state = (TessAppendState *) css;

	tess_shared_stats_reset(state->stats);
	reset_shared(state);
	/*
	 * The children are unfinished again: the leader, which found the valid
	 * ones before, finds them anew to finish the others, where the core's
	 * would leave that to the workers, new in every scan.
	 */
	if (state->exec_prune || state->join_set)
	{
		bms_free(state->valid);
		state->valid = NULL;
		state->valid_known = false;
	}
}

static void
append_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessAppendState *state = (TessAppendState *) css;

	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt, coordinate,
											ParallelWorkerNumber + 1);
	state->shared = (AppendShared *) ((char *) coordinate +
									  tess_shared_stats_size(coordinate));
}

static void
append_shutdown(CustomScanState *css)
{
	TessAppendState *state = (TessAppendState *) css;
	uint64		values[APPEND_NCOUNTERS] = {state->batches};

	if (state->stats != NULL)
		tess_shared_stats_store(state->stats, values);
}

/*
 * As ExecSetTupleBound for an Append: any child may give all the rows the
 * parent needs, so every child gets the bound.
 */
static void
append_set_tuple_bound(CustomScanState *css, int64 tuples_needed)
{
	TessAppendState *state = (TessAppendState *) css;

	for (int index = 0; index < state->nchildren; index++)
		tess_set_child_bound(state->children[index], tuples_needed);
}

static const CustomExecMethods append_exec_methods = {
	.CustomName = "TessAppend",
	.BeginCustomScan = append_begin,
	.ExecCustomScan = append_exec,
	.EndCustomScan = append_end,
	.ReScanCustomScan = append_rescan,
	.ExplainCustomScan = append_explain,
	.EstimateDSMCustomScan = append_estimate_dsm,
	.InitializeDSMCustomScan = append_initialize_dsm,
	.ReInitializeDSMCustomScan = append_reinitialize_dsm,
	.InitializeWorkerCustomScan = append_initialize_worker,
	.ShutdownCustomScan = append_shutdown,
};

const CustomScanMethods tess_append_scan_methods = {
	.CustomName = "TessAppend",
	.CreateCustomScanState = append_create_state,
};

/*
 * Pruning by a hash join's keys (docs/nodes.md, TessHashJoin): the join
 * above hands the node the descriptions it planned for the relation's
 * partitions, by its parameters, and the node makes their pruning states
 * as its own (prune_start: over every planned child, whose numbers the
 * join's keys then prune). False where node is not TessAppend.
 */
bool
tess_append_join_prune_begin(PlanState *node, const PartitionPruneInfo *values,
							 const PartitionPruneInfo *range, const int *params)
{
	TessAppendState *state = (TessAppendState *) node;
	Bitmapset  *all;

	if (tess_batch_node_of(node) != &tess_append_node || values == NULL)
		return false;
	all = bms_add_range(NULL, 0, state->nplanned - 1);
	state->join_values = prune_start(state, values, &all);
	if (range != NULL)
		state->join_range = prune_start(state, range, &all);
	memcpy(state->join_params, params, sizeof(state->join_params));
	return true;
}

/* A key's value as a parameter of the join's pruning. */
static void
set_join_param(TessAppendState *state, int which, int64 value, bool int8)
{
	ParamExecData *param = &state->css.ss.ps.state->es_param_exec_vals[state->join_params[which]];

	param->execPlan = NULL;
	param->value = int8 ? Int64GetDatum(value) : Int32GetDatum((int32) value);
	param->isnull = false;
}

static int
compare_keys(const void *a, const void *b)
{
	int64		left = *(const int64 *) a;
	int64		right = *(const int64 *) b;

	return left < right ? -1 : left > right ? 1 : 0;
}

/*
 * The join built its table: the children its keys may pair with, before
 * the node's first choice. Each distinct key of a short list prunes by the
 * description of `key = $p`, the children it leaves added up, until every
 * child is needed; past the list the lowest and the highest by the one of
 * the range, where the key's family has one; a key the join did not see
 * leaves no child.
 */
void
tess_append_join_prune(PlanState *node, const TessJoinKeys *keys)
{
	TessAppendState *state = (TessAppendState *) node;
	Bitmapset  *planned = NULL;
	int			child = -1;

	if (keys->rows == 0)
		planned = NULL;
	else if (keys->nvalues >= 0)
	{
		qsort(keys->values, keys->nvalues, sizeof(int64), compare_keys);
		for (int index = 0; index < keys->nvalues; index++)
		{
			if (index > 0 && keys->values[index] == keys->values[index - 1])
				continue;
			set_join_param(state, 0, keys->values[index], keys->int8);
			planned = bms_join(planned, ExecFindMatchingSubPlans(state->join_values, false, NULL));
			if (bms_num_members(planned) >= state->nplanned)
				break;
		}
	}
	else if (state->join_range != NULL)
	{
		set_join_param(state, 1, keys->min, keys->int8);
		set_join_param(state, 2, keys->max, keys->int8);
		planned = ExecFindMatchingSubPlans(state->join_range, false, NULL);
	}
	else
		planned = bms_add_range(NULL, 0, state->nplanned - 1);
	bms_free(state->join_valid);
	state->join_valid = NULL;
	while ((child = bms_next_member(planned, child)) >= 0)
	{
		if (child < state->nplanned && state->planned_child[child] >= 0)
			state->join_valid = bms_add_member(state->join_valid, state->planned_child[child]);
	}
	bms_free(planned);
	state->join_removed = state->nchildren - bms_num_members(state->join_valid);
	state->join_set = true;
	bms_free(state->valid);
	state->valid = NULL;
	state->valid_known = false;
}

/*
 * A parent's key filter, handed to every child it reads in the child's own
 * columns: the node takes it only when every child does, since the parent
 * checks no more rows once a node below took it; otherwise the children
 * that took it give it back. NULL takes it back from all.
 */
static bool
append_set_key_filter(CustomScanState *css, const TessKeyFilter *filter)
{
	TessAppendState *state = (TessAppendState *) css;
	int			columns[TESS_TABLE_MAX_KEYS];
	int			taken = 0;

	if (filter == NULL)
	{
		for (int index = 0; index < state->nchildren; index++)
			(void) tess_input_set_key_filter(state->inputs[index], NULL);
		return true;
	}
	if (state->nchildren < 1 || filter->nkeys < 1 || filter->nkeys > TESS_TABLE_MAX_KEYS)
		return false;
	for (; taken < state->nchildren; taken++)
	{
		TessKeyFilter child = *filter;
		bool		mapped = true;

		for (int key = 0; key < filter->nkeys; key++)
		{
			if (filter->columns[key] < 0 || filter->columns[key] >= state->ncolumns)
				mapped = false;
			else
				columns[key] = tess_layout_column(&state->layouts[taken], filter->columns[key]);
		}
		child.columns = columns;
		if (!mapped || !tess_input_set_key_filter(state->inputs[taken], &child))
			break;
	}
	if (taken == state->nchildren)
		return true;
	for (int index = 0; index < taken; index++)
		(void) tess_input_set_key_filter(state->inputs[index], NULL);
	return false;
}

const TessNode tess_append_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_APPEND_NODE_NAME,
	.set_tuple_bound = append_set_tuple_bound,
	.set_key_filter = append_set_key_filter,
	.wrap_append = append_wrap,
};

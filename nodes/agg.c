#include "postgres.h"

#include "access/parallel.h"
#include "catalog/pg_aggregate.h"
#include "catalog/pg_type_d.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "common/int.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/clauses.h"
#include "optimizer/planner.h"
#include "optimizer/tlist.h"
#include "storage/shm_toc.h"
#include "utils/fmgroids.h"
#include "utils/memutils.h"
#include "utils/regproc.h"
#include "utils/ruleutils.h"

#include "tessera/expr.h"
#include "tessera/function.h"
#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

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
#define AGG_COST_FACTOR 0.9
/* A batch with at most this many survivors is gathered for one call later. */
#define AGG_GATHER_ROWS 8
/* Groups per output batch. */
#define AGG_GROUP_ROWS 64
/* The table's first capacity when the planner expects fewer groups. */
#define AGG_INITIAL_GROUPS 256
/* A group's aggregate states have one flag bit each in a payload word. */
#define AGG_MAX_GROUPED 64

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
	AGG_NCOUNTERS
};

/* How the partials of an aggregate combine, and what an empty input gives. */
typedef enum AggKind
{
	AGG_COUNT,					/* int8 sum of the partials, 0 without any */
	AGG_SUM,					/* int8 sum of the partials, NULL without any */
	AGG_MIN,					/* the least partial, NULL without any */
	AGG_MAX						/* the greatest partial, NULL without any */
} AggKind;

typedef struct AggValue
{
	AggKind		kind;
	const TessFunction *function;
	/* The argument's computed column of the projection, or -1 for count(*). */
	int			computed;
	/* The argument's values of sparse batches, a column of their own. */
	Datum	   *gathered_values;
	bool	   *gathered_isnull;
	int			ngathered;
	int64		total;
	/* The extreme so far, int4 or int8 as the aggregate's transition type. */
	int64		extreme;
	bool		wide;
	bool		has_value;
	/* GROUP BY: how the table folds a row into the group's state. */
	TessTableAccumulate accumulate;
} AggValue;

typedef struct TessAggState
{
	CustomScanState css;
	PlanState  *child;
	TessInput  *input;
	TessOutput *output;
	TessBuilder *builder;
	/* The arguments as computed columns after the child's; NULL without any. */
	TessProjection *projection;
	TessLayout	child_layout;
	AggValue   *values;
	int			nvalues;
	/* Written by a batch function on failure only. */
	TessStatus	status;
	/* The row was returned; the next call ends the scan. */
	bool		done;
	/* Under a Gather: the values as they are, for the Finalize Aggregate. */
	bool		partial;
	uint64		batches;
	uint64		rows;
	uint64		calls;
	/* The counters of every participant, in a parallel plan. */
	TessSharedStats *stats;

	/*
	 * GROUP BY: the keys, computed columns before the arguments, and the
	 * table of groups: a record per group, its payload a word of flags
	 * (bit i: aggregate i has a value) and a word per aggregate.
	 */
	int			nkeys;
	TessTableKeyKind kinds[TESS_TABLE_MAX_KEYS];
	const TessKernelOps *kernels;
	MemoryContext table_context;
	/*
	 * The table: its index and chunks, the chunks' bases and lengths with
	 * room for chunk_slots of them, and the bytes they take together.
	 */
	TessTableRef table;
	void	  **chunk_bases;
	Size	   *chunk_lens;
	int			chunk_slots;
	Size		table_bytes;
	Size		peak_memory;
	uint64		groups_estimate;
	uint64		grows;
	uint64		groups;
	/* Per batch: the hashes, the record of each row and the masks. */
	int			capacity;
	uint32	   *hashes;
	uint32	   *offsets;
	uint64	   *valid_bits;
	uint64	   *pending_bits;
	uint64	   *inserted_bits;
	TessDatumColumn key_columns[TESS_TABLE_MAX_KEYS];
	TessTableKey table_keys[TESS_TABLE_MAX_KEYS];
	/* The output: the walk over the groups, the batch a row parent reads. */
	bool		drained;
	uint64		cursor;
	uint32		walked[AGG_GROUP_ROWS];
	Datum		key_values[TESS_TABLE_MAX_KEYS][AGG_GROUP_ROWS];
	bool		key_isnull[TESS_TABLE_MAX_KEYS][AGG_GROUP_ROWS];
	uint64		flag_words[AGG_GROUP_ROWS];
	/* The states of a batch of groups, AGG_GROUP_ROWS per aggregate. */
	uint64	   *state_words;
	TessBatch  *published;
	int			next_row;
} TessAggState;

static const CustomExecMethods agg_exec_methods;
static create_upper_paths_hook_type previous_create_upper_paths_hook = NULL;

static Plan *agg_plan(PlannerInfo *root, RelOptInfo *rel,
					  CustomPath *best_path, List *tlist, List *clauses,
					  List *custom_plans);

static const CustomPathMethods agg_path_methods = {
	.CustomName = "TessAgg",
	.PlanCustomPath = agg_plan,
};

/* The kind of a supported aggregate, or -1: the node knows how to combine these. */
static int
aggregate_kind(Oid aggfnoid)
{
	switch (aggfnoid)
	{
		case F_COUNT_:
		case F_COUNT_ANY:
			return AGG_COUNT;
		case F_SUM_INT4:
			return AGG_SUM;
		case F_MIN_INT4:
		case F_MIN_INT8:
			return AGG_MIN;
		case F_MAX_INT4:
		case F_MAX_INT8:
			return AGG_MAX;
		default:
			return -1;
	}
}

/* The aggregated argument, or NULL for count(*). */
static Node *
aggregate_argument(const Aggref *agg)
{
	return agg->args == NIL ? NULL :
		(Node *) ((TargetEntry *) linitial(agg->args))->expr;
}

/*
 * Whether the node computes this aggregate: a plain call, whole or the
 * partial one of a parallel plan, of an aggregate the node combines and
 * the registry implements over batches, with no argument for count(*),
 * one expression of any type for count (the count reads NULL flags
 * alone) or one int4 or int8 expression for the others, which the
 * projection provider computes by a chain or row by row; a subplan in it
 * would need fixing against the scan tuple, which the arguments do not go
 * through.
 */
static bool
aggregate_supported(const Aggref *agg)
{
	const TessFunction *function;
	Node	   *argument;

	if (agg->agglevelsup != 0 || agg->aggkind != AGGKIND_NORMAL ||
		(agg->aggsplit != AGGSPLIT_SIMPLE &&
		 agg->aggsplit != AGGSPLIT_INITIAL_SERIAL) || agg->aggorder != NIL ||
		agg->aggdistinct != NIL || agg->aggfilter != NULL ||
		agg->aggdirectargs != NIL || agg->aggvariadic ||
		aggregate_kind(agg->aggfnoid) < 0)
		return false;
	function = tess_runtime_api()->functions->find(agg->aggfnoid);
	if (function == NULL || function->kind != TESS_FUNCTION_AGGREGATE)
		return false;
	if (agg->aggfnoid == F_COUNT_)
		return agg->aggstar && agg->args == NIL;
	argument = aggregate_argument(agg);
	if (list_length(agg->args) != 1 || contain_subplans(argument))
		return false;
	return agg->aggfnoid == F_COUNT_ANY || exprType(argument) == INT4OID ||
		exprType(argument) == INT8OID;
}

/*
 * Whether an expression reads something the child's target lacks: a
 * subexpression the target holds whole, as the planner puts a grouping
 * expression there, or a column of it are available; a placeholder
 * would stay one in the private data, so none is accepted.
 */
static bool
unavailable(Node *node, List *exprs)
{
	if (node == NULL)
		return false;
	if (list_member(exprs, node))
		return false;
	if (IsA(node, Var) || IsA(node, PlaceHolderVar))
		return true;
	return expression_tree_walker(node, unavailable, exprs);
}

/* Whether the child's target gives what the keys and the arguments read. */
static bool
arguments_available(const List *tlist, const Path *child)
{
	foreach_ptr(TargetEntry, entry, tlist)
	{
		Node	   *argument = IsA(entry->expr, Aggref) ?
			aggregate_argument((Aggref *) entry->expr) : (Node *) entry->expr;

		if (unavailable(argument, child->pathtarget->exprs))
			return false;
	}
	return true;
}

/*
 * Whether an expression above the aggregation is made of what the scan
 * tuple holds: grouping expressions, aggregates and constants, as the
 * planner will rewrite it; a column outside them, such as one the
 * primary key makes functionally dependent, is not.
 */
static bool
not_from_groups(Node *node, List *keys)
{
	if (node == NULL || IsA(node, Aggref))
		return false;
	foreach_ptr(Node, key, keys)
	{
		if (equal(node, key))
			return false;
	}
	if (IsA(node, Var) || IsA(node, PlaceHolderVar))
		return true;
	return expression_tree_walker(node, not_from_groups, keys);
}

/*
 * Append the distinct aggregates of the expressions to a flat target
 * list, the scan tuple of the node after the grouping expressions; false
 * when one is not supported, or an expression reads a column neither a
 * grouping expression nor an aggregate gives. Expressions above the
 * aggregates are left to the plan's projection and qual over that tuple.
 */
static bool
collect_aggregates(Node *expressions, List *keys, List **tlist)
{
	List	   *found;

	if (keys != NIL && not_from_groups(expressions, keys))
		return false;
	found = pull_var_clause(expressions, PVC_INCLUDE_AGGREGATES |
							PVC_RECURSE_PLACEHOLDERS);
	foreach_ptr(Node, node, found)
	{
		if (IsA(node, Aggref))
		{
			if (!aggregate_supported((Aggref *) node))
				return false;
			*tlist = add_to_flat_tlist(*tlist, list_make1(node));
		}
		else if (keys == NIL)
			return false;
	}
	return true;
}

/*
 * The queries the node handles: plain aggregation of a single result row,
 * or grouping by plain grouping expressions.
 */
static bool
query_supported(PlannerInfo *root, RelOptInfo *input_rel, RelOptInfo *output_rel)
{
	Query	   *parse = root->parse;

	return (parse->hasAggs || parse->groupClause != NIL) &&
		parse->groupingSets == NIL && !parse->hasWindowFuncs &&
		output_rel->reloptkind == RELOPT_UPPER_REL && !IS_DUMMY_REL(input_rel);
}

/*
 * The grouping expressions when the node can group by them: 1 to 16 int4
 * or int8 values the expression compiler takes, a bare column or a chain
 * such as c % 10. NIL otherwise, also when the planner dropped every
 * grouping clause, as for a constant one.
 */
static List *
group_keys(PlannerInfo *root)
{
	List	   *keys = NIL;

	if (root->processed_groupClause == NIL ||
		list_length(root->processed_groupClause) > TESS_TABLE_MAX_KEYS)
		return NIL;
	foreach_node(SortGroupClause, clause, root->processed_groupClause)
	{
		Node	   *expr = (Node *) get_sortgroupclause_expr(clause,
															root->processed_tlist);
		Oid			type = exprType(expr);

		if ((type != INT4OID && type != INT8OID) || contain_subplans(expr) ||
			contain_volatile_functions(expr) || !tess_expr_supports_value(expr, 0))
			return NIL;
		keys = lappend(keys, expr);
	}
	return keys;
}

/*
 * The bytes a table of groups takes by the planner's estimate: a record
 * of a header, a slot per key and a payload of a flags word and a word
 * per aggregate, and the buckets, at least twice the records.
 */
static double
group_bytes(double groups, int nkeys, int naggregates)
{
	double		buckets = 1024;

	groups = Max(groups, 1.0);
	while (buckets < 2 * groups)
		buckets *= 2;
	return groups * (16 + 8 * nkeys + 8 * (1 + naggregates)) + buckets * 4;
}

/* The core's aggregate paths of the list with this strategy and split. */
static List *
aggregate_templates(const List *pathlist, AggStrategy strategy, AggSplit aggsplit)
{
	List	   *templates = NIL;

	foreach_ptr(Path, path, pathlist)
	{
		if (IsA(path, AggPath) &&
			((AggPath *) path)->aggstrategy == strategy &&
			((AggPath *) path)->aggsplit == aggsplit)
			templates = lappend(templates, path);
	}
	return templates;
}

/*
 * The node's path in place of the core's aggregate path: the same planner
 * properties and rows, a lower cost, the batch child over the core path's
 * input, the grouping expressions and the aggregates it computes. NULL
 * when the input cannot be read in batches or lacks a column, or when the
 * planner's estimate of the groups would not fit in hash_mem: the node
 * keeps every group in memory.
 */
static CustomPath *
make_agg_path(PlannerInfo *root, const AggPath *agg, List *tlist, int nkeys)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *child;
	Path		template;

	if (nkeys > 0 &&
		group_bytes(agg->path.rows, nkeys, list_length(tlist) - nkeys) >
		(double) get_hash_memory_limit())
		return NULL;
	child = tess_batch_input_path(root, agg->subpath);
	if (child == NULL || !arguments_available(tlist, child))
		return NULL;
	template = agg->path;
	template.total_cost *= AGG_COST_FACTOR;
	config.template_path = &template;
	config.methods = &agg_path_methods;
	config.node = &tess_agg_node;
	config.children = list_make1(child);
	config.expressions = tlist;
	config.node_data = (Node *) list_make2_int(nkeys,
											   (int) Min(agg->path.rows,
														 (double) PG_INT32_MAX));
	return tess_path_create(&config);
}

/*
 * The partially grouped relation of the grouped one, when the core built
 * partial aggregate paths for it; PostgreSQL passes it to no hook.
 */
static RelOptInfo *
partial_grouping_rel(PlannerInfo *root, RelOptInfo *grouped_rel)
{
	foreach_ptr(RelOptInfo, rel, root->upper_rels[UPPERREL_PARTIAL_GROUP_AGG])
	{
		if (bms_equal(rel->relids, grouped_rel->relids))
			return rel->partial_pathlist != NIL ? rel : NULL;
	}
	return NULL;
}

/*
 * The node in place of the core's partial aggregate under a Gather.
 * PostgreSQL offers extensions no hook for the partially grouped relation
 * and builds the Gather and the Finalize Aggregate before it calls this
 * one, so the node builds the whole stack for each of the core's partial
 * aggregate paths: its own partial path over the batch child, with the
 * partial aggregates as its targets, the core's Gather over it and the
 * core's Finalize Aggregate over that, which combines the participants'
 * values and applies HAVING. With GROUP BY each participant keeps a table
 * of its own groups and the core's Finalize HashAggregate merges them.
 * The path is parallel-aware for the counters the node shares; the child
 * divides the work.
 */
static void
create_partial_paths(PlannerInfo *root, RelOptInfo *grouped_rel,
					 GroupPathExtraData *extra, List *keys, double groups)
{
	RelOptInfo *partial_rel = partial_grouping_rel(root, grouped_rel);
	List	   *tlist = keys != NIL ? add_to_flat_tlist(NIL, keys) : NIL;
	AggStrategy strategy = keys != NIL ? AGG_HASHED : AGG_PLAIN;

	if (partial_rel == NULL || extra == NULL || !extra->partial_costs_set ||
		(keys != NIL && groups <= 0))
		return;
	if (!collect_aggregates((Node *) partial_rel->reltarget->exprs, keys, &tlist) ||
		list_length(tlist) == list_length(keys))
		return;
	foreach_ptr(AggPath, agg, aggregate_templates(partial_rel->partial_pathlist,
												  strategy,
												  AGGSPLIT_INITIAL_SERIAL))
	{
		CustomPath *partial = make_agg_path(root, agg, tlist, list_length(keys));
		GatherPath *gather;
		AggPath    *final;
		double		rows;

		if (partial == NULL || !partial->path.parallel_safe ||
			partial->path.parallel_workers <= 0)
			continue;
		partial->path.parallel_aware = true;
		rows = compute_gather_rows(&partial->path);
		gather = create_gather_path(root, partial_rel, &partial->path,
									partial->path.pathtarget, NULL, &rows);
		/* With GROUP BY the core's Finalize HashAggregate merges the groups. */
		final = create_agg_path(root, grouped_rel, &gather->path,
								grouped_rel->reltarget, strategy,
								AGGSPLIT_FINAL_DESERIAL,
								keys != NIL ? root->processed_groupClause : NIL,
								(List *) extra->havingQual,
								&extra->agg_final_costs,
								keys != NIL ? groups : 1.0);
		add_path(grouped_rel, &final->path);
	}
}

/*
 * The node's path in place of each of the core's plain aggregate paths
 * whose input can be read in batches, and the parallel stack in place of
 * each partial one.
 */
static void
create_upper_paths(PlannerInfo *root, UpperRelationKind stage,
				   RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra)
{
	List	   *keys = NIL;
	List	   *tlist = NIL;
	AggStrategy strategy = AGG_PLAIN;
	double		groups = 0;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel,
										 extra);
	if (!*tess_runtime_api()->settings->enable ||
		stage != UPPERREL_GROUP_AGG ||
		!query_supported(root, input_rel, output_rel))
		return;
	if (root->parse->groupClause != NIL)
	{
		/* The grouping expressions lead the scan tuple, the table's keys. */
		keys = group_keys(root);
		if (keys == NIL)
			return;
		tlist = add_to_flat_tlist(NIL, keys);
		strategy = AGG_HASHED;
	}
	if (!collect_aggregates((Node *) list_make2(output_rel->reltarget->exprs,
												root->parse->havingQual),
							keys, &tlist) ||
		tlist == NIL ||
		(keys != NIL && list_length(tlist) - list_length(keys) > AGG_MAX_GROUPED))
		return;
	/* add_path changes the list: the candidates are taken first. */
	foreach_ptr(AggPath, agg, aggregate_templates(output_rel->pathlist,
												  strategy, AGGSPLIT_SIMPLE))
	{
		CustomPath *path = make_agg_path(root, agg, tlist, list_length(keys));

		if (path != NULL)
			add_path(output_rel, &path->path);
	}
	/*
	 * The planner's estimate of the groups, for a Finalize HashAggregate:
	 * the rows of the core's grouped paths, the relation's own being set
	 * only after this hook.
	 */
	foreach_ptr(Path, path, output_rel->pathlist)
		groups = Max(groups, path->rows);
	create_partial_paths(root, output_rel, (GroupPathExtraData *) extra, keys,
						 groups);
}

/*
 * An argument's column as a Var of INDEX_VAR naming the child's target,
 * which the projection maps to its batch column through the child's
 * layout: the arguments cannot go through custom_exprs, which the planner
 * would fix against the scan tuple of aggregates, so they travel in the
 * private data with their targets resolved here.
 */
static Node *
resolve_argument(Node *node, TessPlanChild *child)
{
	TargetEntry *found;

	if (node == NULL)
		return NULL;
	/* A column of the child, or an expression it computes, as a grouping one. */
	found = IsA(node, Const) ? NULL :
		tlist_member((Expr *) node, child->plan->targetlist);
	if (found != NULL)
	{
		if (tess_layout_column(&child->layout, found->resno - 1) < 0)
			elog(ERROR, "TessAgg argument is missing from its child");
		return (Node *) makeVar(INDEX_VAR, found->resno, exprType(node),
								exprTypmod(node), exprCollation(node), 0);
	}
	if (IsA(node, Var))
		elog(ERROR, "TessAgg argument is missing from its child");
	return expression_tree_mutator(node, resolve_argument, child);
}

/*
 * The parameters of the arguments, which the planner must see in
 * custom_exprs to count them among the plan's: a node above that
 * rescans its child only when its parameters changed would otherwise
 * keep a stale result.
 */
static bool
collect_params(Node *node, List **params)
{
	if (node == NULL)
		return false;
	if (IsA(node, Param))
	{
		*params = lappend(*params, node);
		return false;
	}
	return expression_tree_walker(node, collect_params, params);
}

/*
 * The scan tuple is the aggregates themselves, so that the planner turns
 * the targets and HAVING into references to it; the child's columns stay
 * hidden. HAVING is the plan's qual, as it is the core aggregate's, except
 * for the partial aggregates of a parallel plan, whose Finalize Aggregate
 * applies it. The private data carries one argument per aggregate, a NULL
 * constant for count(*).
 */
static Plan *
agg_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		 List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);
	List	   *arguments = NIL;
	List	   *keys = NIL;
	List	   *params = NIL;
	List	   *path_data;
	TessPlanWriter *writer;
	bool		partial = false;
	int			nkeys;

	tess_path_get_info(best_path, &info);
	if (!tess_plan_child(best_path, custom_plans, 0, &child))
		elog(ERROR, "TessAgg expected a batch child");
	path_data = (List *) info.node_data;
	nkeys = linitial_int(path_data);
	foreach_ptr(TargetEntry, entry, info.expressions)
	{
		Node	   *argument;

		/* The grouping expressions, then the aggregates. */
		if (foreach_current_index(entry) < nkeys)
		{
			keys = lappend(keys, resolve_argument((Node *) copyObject(entry->expr),
												  &child));
			continue;
		}
		partial = DO_AGGSPLIT_SKIPFINAL(((Aggref *) entry->expr)->aggsplit);
		argument = aggregate_argument((Aggref *) entry->expr);
		arguments = lappend(arguments, argument == NULL ?
							(Node *) makeNullConst(INT4OID, -1, InvalidOid) :
							resolve_argument(copyObject(argument), &child));
	}
	collect_params((Node *) arguments, &params);
	collect_params((Node *) keys, &params);
	writer = tess_plan_writer_create(TESS_AGG_DATA, TESS_AGG_DATA_VERSION);
	tess_plan_write_list(writer, "arguments", arguments);
	tess_plan_write_list(writer, "keys", keys);
	tess_plan_write_int(writer, "groups", lsecond_int(path_data));
	config.methods = &tess_agg_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.qual = partial ? NIL : (List *) root->parse->havingQual;
	config.expressions = params;
	config.scan_targetlist = info.expressions;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

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
	List	   *keys;
	TessPlanReader *reader;
	int			groups;
	int			index = 0;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessAgg supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_agg_node || info.nchildren != 1 ||
		info.child_names[0] == NULL || cscan->custom_scan_tlist == NIL)
		elog(ERROR, "TessAgg received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, TESS_AGG_DATA,
									 TESS_AGG_DATA_VERSION);
	arguments = tess_plan_read_list(reader, "arguments");
	keys = tess_plan_read_list(reader, "keys");
	groups = tess_plan_read_int(reader, "groups");
	state->groups_estimate = (uint64) Max(groups, 0);
	tess_plan_reader_finish(reader);
	state->nkeys = list_length(keys);
	if (state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(arguments) + state->nkeys != list_length(cscan->custom_scan_tlist) ||
		(state->nkeys == 0 && arguments == NIL) ||
		list_length(arguments) > AGG_MAX_GROUPED)
		elog(ERROR, "TessAgg received a foreign plan");
	state->child = ExecInitNode(child_plan, estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->input = tess_input_create(estate->es_query_cxt, state->child);
	state->child_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	tess_plan_get_layout(child_plan, &state->child_layout);
	state->nvalues = list_length(arguments);
	state->values = palloc0_array(AggValue, Max(state->nvalues, 1));
	state->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	/* The keys are the first computed columns, the table's key kinds. */
	foreach_ptr(Node, key, keys)
	{
		int			position = foreach_current_index(key);

		state->kinds[position] = exprType(key) == INT8OID ?
			TESS_TABLE_KEY_INT8 : TESS_TABLE_KEY_INT4;
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
		state->partial = DO_AGGSPLIT_SKIPFINAL(agg->aggsplit);
		value->kind = aggregate_kind(agg->aggfnoid);
		value->wide = agg->aggtranstype == INT8OID;
		value->function = tess_runtime_api()->functions->find(agg->aggfnoid);
		if (value->kind < 0 || value->function == NULL ||
			value->function->kind != TESS_FUNCTION_AGGREGATE)
			elog(ERROR, "TessAgg has no batch implementation of %s",
				 format_procedure(agg->aggfnoid));
		value->computed = -1;
		switch (value->kind)
		{
			case AGG_COUNT:
				value->accumulate = agg->args == NIL ? TESS_TABLE_COUNT_ROWS :
					TESS_TABLE_COUNT;
				break;
			case AGG_SUM:
				value->accumulate = TESS_TABLE_SUM_INT4;
				break;
			case AGG_MIN:
				value->accumulate = value->wide ? TESS_TABLE_MIN_INT8 :
					TESS_TABLE_MIN_INT4;
				break;
			case AGG_MAX:
				value->accumulate = value->wide ? TESS_TABLE_MAX_INT8 :
					TESS_TABLE_MAX_INT4;
				break;
		}
		if (agg->args != NIL)
		{
			Node	   *argument = list_nth(arguments, index - 1);
			List	   *vars = pull_var_clause(argument, 0);

			value->computed = list_length(computed);
			computed = lappend(computed,
							   makeTargetEntry((Expr *) argument,
											   value->computed + 1, NULL, false));
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
		}
	}
	if (computed != NIL)
	{
		/* The arguments are computed columns over the child's target list. */
		TessProjectionConfig config = TESS_STRUCT_INITIALIZER(TessProjectionConfig);

		config.parent_context = estate->es_query_cxt;
		config.parent = &css->ss.ps;
		config.econtext = css->ss.ps.ps_ExprContext;
		config.scan_slot = ExecInitExtraTupleSlot(estate,
												  ExecTypeFromTL(child_plan->targetlist),
												  &TTSOpsVirtual);
		config.scan_tuple = &state->child_layout;
		config.base_columns = state->child_layout.ncolumns;
		config.computed = computed;
		state->projection = tess_projection_create(&config);
	}
	/* Whole batches; the arguments' columns only for the surviving rows. */
	request.projection_columns = projection;
	request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->input, &request);
	builder.parent_context = estate->es_query_cxt;
	builder.tuple_desc = result->tts_tupleDescriptor;
	builder.ncolumns = result->tts_tupleDescriptor->natts;
	builder.capacity = state->nkeys > 0 ? AGG_GROUP_ROWS : 1;
	if (state->nkeys > 0)
	{
		state->kernels = tess_runtime_kernels();
		if (state->kernels == NULL ||
			!TESS_ABI_HAS_FIELD(state->kernels, TessKernelOps, table_gather_key))
			elog(ERROR, "TessAgg needs the kernels module for GROUP BY");
		state->table_context = AllocSetContextCreate(estate->es_query_cxt,
													 "TessAgg groups",
													 ALLOCSET_DEFAULT_SIZES);
		state->state_words = palloc0_array(uint64,
										   Max(state->nvalues, 1) * AGG_GROUP_ROWS);
		if (state->projection == NULL)
			elog(ERROR, "TessAgg groups by computed columns");
	}
	state->builder = tess_builder_create(&builder);
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   result, &info.layout);
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
	}
	value->has_value = true;
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
	int			row = -1;

	if (value->computed < 0)
	{
		evaluate(state, value, NULL, &batch->rows);
		return;
	}
	batch->ops->get_datum_column(batch,
								 state->child_layout.ncolumns + value->computed,
								 &batch->rows, TESS_COLUMN_FOR_PROJECTION,
								 &computed);
	if (computed.values == NULL || computed.isnull == NULL ||
		computed.nrows != batch->rows.nrows)
		elog(ERROR, "Tessera projection returned an invalid column");
	if (nrows > AGG_GATHER_ROWS)
	{
		evaluate(state, value, column, &batch->rows);
		return;
	}
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		if (value->ngathered == 64)
			flush_gathered(state, value);
		value->gathered_values[value->ngathered] = column->values[row];
		value->gathered_isnull[value->ngathered] = column->isnull[row];
		value->ngathered++;
	}
}

/* Read every batch of the child into the running values. */
static void
drain(TessAggState *state)
{
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
		}
	}
	return ExecStoreVirtualTuple(scan);
}

/* Raise the error a table call stored, if it failed. */
static inline void
check(TessAggState *state, TessStatusCode code)
{
	if (code != TESS_OK)
		tess_status_report(&state->status);
}

/* The bytes of the table now, and the most so far. */
static void
note_memory(TessAggState *state)
{
	state->peak_memory = Max(state->peak_memory, state->table_bytes);
}

/*
 * The groups' records lie in chunks: the first of AGG_FIRST_CHUNK bytes,
 * so that a few groups take little, the others of the most a chunk may
 * have. Records never move; when they reach half the buckets, only the
 * index is made anew, larger.
 */
#define AGG_FIRST_CHUNK (64 * 1024)

/* An index for capacity groups in the table's memory. */
static void *
new_index(TessAggState *state, uint64 capacity, Size *size)
{
	Size		payload_size = sizeof(uint64) * (1 + state->nvalues);

	check(state, state->kernels->table_size(state->nkeys, state->kinds,
											payload_size, capacity, size,
											&state->status));
	return MemoryContextAllocExtended(state->table_context, *size, MCXT_ALLOC_HUGE);
}

/* An empty table of groups, its index sized for the planner's estimate. */
static void
create_table(TessAggState *state)
{
	Size		payload_size = sizeof(uint64) * (1 + state->nvalues);
	uint64		capacity = Max(state->groups_estimate, AGG_INITIAL_GROUPS);
	Size		size;

	MemoryContextReset(state->table_context);
	state->capacity = 0;
	state->chunk_slots = 16;
	state->chunk_bases = MemoryContextAlloc(state->table_context,
											sizeof(void *) * state->chunk_slots);
	state->chunk_lens = MemoryContextAlloc(state->table_context,
										   sizeof(Size) * state->chunk_slots);
	state->table.index = new_index(state, capacity, &size);
	state->table.index_len = size;
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
	state->table.nchunks = 0;
	state->table_bytes = size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys,
											  state->kinds, payload_size,
											  capacity, &state->status));
	note_memory(state);
}

/* Another chunk of records, the last one being full. */
static void
add_chunk(TessAggState *state)
{
	int			chunk = state->table.nchunks;
	Size		len = chunk == 0 ? AGG_FIRST_CHUNK : TESS_TABLE_MAX_CHUNK_LEN;
	void	   *base;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessAgg group table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	if (chunk == state->chunk_slots)
	{
		state->chunk_slots *= 2;
		state->chunk_bases = repalloc(state->chunk_bases,
									  sizeof(void *) * state->chunk_slots);
		state->chunk_lens = repalloc(state->chunk_lens,
									 sizeof(Size) * state->chunk_slots);
		state->table.chunks = state->chunk_bases;
		state->table.chunk_lens = state->chunk_lens;
	}
	base = MemoryContextAlloc(state->table_context, len);
	check(state, state->kernels->table_chunk_init(base, len, &state->status));
	state->chunk_bases[chunk] = base;
	state->chunk_lens[chunk] = len;
	state->table.nchunks++;
	state->table_bytes += len;
	note_memory(state);
}

/*
 * An index for twice the groups: the buckets are filled anew from the
 * records, which stay where they are, and the old index is freed.
 */
static void
regrow_table(TessAggState *state, uint64 groups)
{
	void	   *old = state->table.index;
	Size		size;
	void	   *index = new_index(state, groups * 2, &size);

	check(state, state->kernels->table_regrow(&state->table, index, size,
											  groups * 2, &state->status));
	state->table_bytes = state->table_bytes - state->table.index_len + size;
	state->peak_memory = Max(state->peak_memory, state->table_bytes +
							 state->table.index_len);
	state->table.index = index;
	state->table.index_len = size;
	pfree(old);
	state->grows++;
}

/* The buffers of a batch of nrows rows, in the table's memory. */
static void
reserve_rows(TessAggState *state, int nrows)
{
	int			nwords = tess_row_mask_word_count(nrows);

	if (state->capacity >= nrows)
		return;
	state->hashes = MemoryContextAlloc(state->table_context, sizeof(uint32) * nrows);
	state->offsets = MemoryContextAlloc(state->table_context, sizeof(uint32) * nrows);
	state->valid_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->pending_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->inserted_bits = MemoryContextAlloc(state->table_context, sizeof(uint64) * nwords);
	state->capacity = nrows;
}

/* A computed column of the projection's wrapper, checked. */
static void
computed_column(TessAggState *state, TessBatch *batch, int computed,
				TessColumnPurpose purpose, TessDatumColumn *result)
{
	*result = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	batch->ops->get_datum_column(batch, state->child_layout.ncolumns + computed,
								 &batch->rows, purpose, result);
	if (result->values == NULL || result->isnull == NULL ||
		result->nrows != batch->rows.nrows)
		elog(ERROR, "Tessera projection returned an invalid column");
}

/*
 * One batch into the groups: its keys, hashed in key order with NULL as a
 * key of its own, give each row the record of its group, created where
 * none exists (in another chunk or a larger index when the table has no
 * room), and each aggregate folds the
 * rows into the records' states. The batch is the projection's wrapper,
 * which computes the keys and the arguments.
 */
static void
group_batch(TessAggState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask pending;
	TessRowMask inserted;

	reserve_rows(state, nrows);
	memset(state->valid_bits, 0, sizeof(uint64) * nwords);
	memset(state->inserted_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->valid_bits};
	pending = (TessRowMask) {nrows, state->pending_bits};
	inserted = (TessRowMask) {nrows, state->inserted_bits};
	for (int key = 0; key < state->nkeys; key++)
	{
		TessDatumColumn *column = &state->key_columns[key];
		bool		int8 = state->kinds[key] == TESS_TABLE_KEY_INT8;

		computed_column(state, batch, key, TESS_COLUMN_FOR_FILTER, column);
		if (key == 0)
			check(state, (int8 ? state->kernels->int8_hash :
						  state->kernels->int4_hash) (column, NULL, &batch->rows,
													  TESS_NULL_KEYS_GROUP,
													  state->hashes, &valid,
													  &state->status));
		else
			check(state, (int8 ? state->kernels->int8_hash_next :
						  state->kernels->int4_hash_next) (column, NULL,
														   TESS_NULL_KEYS_GROUP,
														   state->hashes, &valid,
														   &state->status));
		state->table_keys[key].kind = state->kinds[key];
		state->table_keys[key].column = column;
		state->table_keys[key].prepared = NULL;
	}
	memcpy(state->pending_bits, state->valid_bits, sizeof(uint64) * nwords);
	if (state->table.nchunks == 0)
		add_chunk(state);
	for (;;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

		check(state, state->kernels->table_find_or_insert(&state->table,
														  state->table.nchunks - 1,
														  state->hashes,
														  state->nkeys,
														  state->table_keys,
														  &pending,
														  state->offsets,
														  &inserted,
														  &state->status));
		if (tess_row_mask_count(&pending) == 0)
			break;
		/*
		 * The rows left pending find room in a larger index, when the
		 * groups reached half the buckets, or else in another chunk.
		 */
		check(state, state->kernels->table_stats(&state->table, &stats,
												 &state->status));
		if (stats.records * 2 >= stats.buckets)
			regrow_table(state, stats.records);
		else
			add_chunk(state);
	}
	for (int index = 0; index < state->nvalues; index++)
	{
		AggValue   *value = &state->values[index];
		TessDatumColumn column;

		if (value->computed >= 0)
			computed_column(state, batch, value->computed,
							TESS_COLUMN_FOR_PROJECTION, &column);
		state->calls++;
		check(state, state->kernels->table_accumulate(&state->table,
													  state->offsets, &valid,
													  value->accumulate,
													  value->computed >= 0 ? &column : NULL,
													  NULL,
													  sizeof(uint64) * (1 + index),
													  0, (uint32) index,
													  &state->status));
	}
}

/* Read every batch of the child into the table of groups. */
static void
group_drain(TessAggState *state)
{
	create_table(state);
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
			TessBatch  *input = tess_projection_wrap(state->projection, batch);

			ResetExprContext(state->css.ss.ps.ps_ExprContext);
			group_batch(state, input);
			input->ops->release(input);
		}
		tess_input_finish(state->input);
	}
	state->drained = true;
	state->cursor = 0;
}

/*
 * Aggregate index of a group into the scan slot: a count as it is, a sum
 * or an extreme NULL without the flag of a value, an int4 extreme as an
 * int4 Datum.
 */
static void
group_value(TessAggState *state, int index, int group, TupleTableSlot *scan)
{
	AggValue   *value = &state->values[index];
	uint64		word = state->state_words[index * AGG_GROUP_ROWS + group];
	bool		seen = ((state->flag_words[group] >> index) & 1) != 0;
	int			attribute = state->nkeys + index;

	scan->tts_isnull[attribute] = value->kind != AGG_COUNT && !seen;
	switch (value->kind)
	{
		case AGG_COUNT:
		case AGG_SUM:
			scan->tts_values[attribute] = Int64GetDatum((int64) word);
			break;
		case AGG_MIN:
		case AGG_MAX:
			scan->tts_values[attribute] = value->wide ?
				Int64GetDatum((int64) word) : Int32GetDatum((int32) (int64) word);
			break;
	}
}

/*
 * The next groups of the walk, up to a batch of them, as result rows:
 * the keys and the aggregates in the scan slot, HAVING over them and the
 * plan's projection, as for the one row without GROUP BY. NULL when the
 * walk is over; a batch HAVING left empty is not returned.
 */
static TessBatch *
next_groups(TessAggState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;
	TupleTableSlot *scan = state->css.ss.ss_ScanTupleSlot;

	for (;;)
	{
		uint64		all;
		TessRowMask groups;
		int			count;
		TessBatch  *batch;

		check(state, state->kernels->table_scan(&state->table,
												&state->cursor, state->walked,
												AGG_GROUP_ROWS, &count,
												&state->status));
		if (count == 0)
			return NULL;
		state->groups += count;
		all = count == 64 ? UINT64_MAX : (UINT64CONST(1) << count) - 1;
		groups = (TessRowMask) {count, &all};
		for (int key = 0; key < state->nkeys; key++)
			check(state, state->kernels->table_gather_key(&state->table,
														  state->walked, &groups,
														  key, state->key_values[key],
														  state->key_isnull[key],
														  &state->status));
		check(state, state->kernels->table_gather(&state->table,
												  state->walked, &groups, 0,
												  (Datum *) state->flag_words,
												  &state->status));
		tess_builder_reset(state->builder);
		for (int index = 0; index < state->nvalues; index++)
			check(state, state->kernels->table_gather(&state->table,
													  state->walked, &groups,
													  sizeof(uint64) * (1 + index),
													  (Datum *) &state->state_words[index * AGG_GROUP_ROWS],
													  &state->status));
		for (int group = 0; group < count; group++)
		{
			TupleTableSlot *row;

			ExecClearTuple(scan);
			for (int key = 0; key < state->nkeys; key++)
			{
				scan->tts_values[key] = state->key_values[key][group];
				scan->tts_isnull[key] = state->key_isnull[key][group];
			}
			for (int index = 0; index < state->nvalues; index++)
				group_value(state, index, group, scan);
			ExecStoreVirtualTuple(scan);
			ResetExprContext(econtext);
			econtext->ecxt_scantuple = scan;
			if (state->css.ss.ps.qual != NULL &&
				!ExecQual(state->css.ss.ps.qual, econtext))
				continue;
			row = state->css.ss.ps.ps_ProjInfo != NULL ?
				ExecProject(state->css.ss.ps.ps_ProjInfo) :
				ExecCopySlot(state->css.ss.ps.ps_ResultTupleSlot, scan);
			tess_builder_append_slot(state->builder, row);
		}
		batch = tess_builder_finish(state->builder, InvalidOid);
		if (batch != NULL)
			return batch;
	}
}

/*
 * The result row, once: the aggregates in the scan slot, HAVING over
 * them, and the plan's projection when the targets are not the bare
 * aggregates, as the executor set it up for the scan tuple.
 */
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
		group_drain(state);
	if (rows && state->published != NULL)
	{
		state->next_row = tess_row_mask_next(&state->published->rows,
											 state->next_row);
		if (state->next_row >= 0)
			return tess_output_select(state->output, state->next_row);
		tess_output_finish(state->output);
	}
	tess_output_release(state->output);
	state->published = next_groups(state);
	if (state->published == NULL)
	{
		state->done = true;
		return NULL;
	}
	state->next_row = tess_row_mask_next(&state->published->rows, -1);
	return tess_output_publish(state->output, state->published);
}

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
	ExecEndNode(state->child);
	if (state->table_context != NULL)
		MemoryContextDelete(state->table_context);
}

static void
agg_rescan(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;

	tess_output_clear(state->output);
	if (state->projection != NULL)
		tess_projection_reset(state->projection);
	/* The core passes changed parameters to outer and inner plans only. */
	if (css->ss.ps.chgParam != NULL)
		UpdateChangedParamSet(state->child, css->ss.ps.chgParam);
	ExecReScan(state->child);
	tess_input_rescan(state->input);
	for (int index = 0; index < state->nvalues; index++)
	{
		state->values[index].total = 0;
		state->values[index].has_value = false;
		state->values[index].ngathered = 0;
	}
	state->done = false;
	state->batches = 0;
	state->rows = 0;
	state->calls = 0;
	/* GROUP BY: the table is built again from the rescanned child. */
	state->drained = false;
	state->published = NULL;
	state->groups = 0;
}

/* This participant's counters. */
static void
agg_counters(TessAggState *state, uint64 *values)
{
	memset(values, 0, AGG_NCOUNTERS * sizeof(uint64));
	values[AGG_BATCHES] = state->batches;
	values[AGG_ROWS] = state->rows;
	values[AGG_CALLS] = state->calls;
	if (state->projection != NULL)
	{
		const TessProjectionStats *computed = tess_projection_stats(state->projection);

		values[AGG_COMPUTED] = computed->chain_datums + computed->row_datums;
	}
	values[AGG_GROUPS] = state->groups;
	values[AGG_MEMORY] = state->peak_memory;
	values[AGG_GROWS] = state->grows;
}

/* The totals of every participant in a parallel plan, else the node's own. */
static void
agg_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessAggState *state = (TessAggState *) css;
	const uint64 *totals = NULL;
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
	}
	if (state->partial)
		ExplainPropertyText("Partial Mode", "Partial", es);
	if (!es->analyze)
		return;
	if (state->stats != NULL)
		totals = tess_shared_stats_totals(state->stats);
	if (totals == NULL)
	{
		agg_counters(state, own);
		totals = own;
	}
	ExplainPropertyInteger("Input Batches", NULL, totals[AGG_BATCHES], es);
	ExplainPropertyInteger("Input Rows", NULL, totals[AGG_ROWS], es);
	ExplainPropertyInteger("Kernel Calls", NULL, totals[AGG_CALLS], es);
	if (state->projection != NULL)
		ExplainPropertyInteger("Computed Datums", NULL, totals[AGG_COMPUTED], es);
	if (state->nkeys > 0)
	{
		ExplainPropertyInteger("Groups", NULL, totals[AGG_GROUPS], es);
		ExplainPropertyInteger("Table Grows", NULL, totals[AGG_GROWS], es);
		ExplainPropertyInteger("Memory Usage", "kB",
							   (totals[AGG_MEMORY] + 1023) / 1024, es);
		/* The node keeps every group in memory rather than spilling. */
		if (totals[AGG_MEMORY] > get_hash_memory_limit())
			ExplainPropertyInteger("Overrun", "kB",
								   (totals[AGG_MEMORY] - get_hash_memory_limit() + 1023) / 1024,
								   es);
	}
}

/*
 * A parallel plan: the node shares only its counters, in the rows of its
 * chunk; the child divides the work and the Finalize Aggregate above the
 * Gather combines the participants' values.
 */
static Size
agg_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	return tess_shared_stats_estimate(AGG_NCOUNTERS, pcxt->nworkers);
}

static void
agg_initialize_dsm(CustomScanState *css, ParallelContext *pcxt,
				   void *coordinate)
{
	TessAggState *state = (TessAggState *) css;

	/* A Gather a limit above shut down sets up anew when rescanned. */
	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	state->stats = tess_shared_stats_init(css->ss.ps.state->es_query_cxt,
										  coordinate, AGG_NCOUNTERS,
										  pcxt->nworkers, pcxt->seg);
}

static void
agg_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt,
					 void *coordinate)
{
	TessAggState *state = (TessAggState *) css;

	tess_shared_stats_reset(state->stats);
}

static void
agg_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessAggState *state = (TessAggState *) css;

	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt,
											coordinate, ParallelWorkerNumber + 1);
}

static void
agg_shutdown(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;
	uint64		values[AGG_NCOUNTERS];

	if (state->stats == NULL)
		return;
	agg_counters(state, values);
	tess_shared_stats_store(state->stats, values);
}

static const CustomExecMethods agg_exec_methods = {
	.CustomName = "TessAgg",
	.BeginCustomScan = agg_begin,
	.ExecCustomScan = agg_exec,
	.EndCustomScan = agg_end,
	.ReScanCustomScan = agg_rescan,
	.ExplainCustomScan = agg_explain,
	.EstimateDSMCustomScan = agg_estimate_dsm,
	.InitializeDSMCustomScan = agg_initialize_dsm,
	.ReInitializeDSMCustomScan = agg_reinitialize_dsm,
	.InitializeWorkerCustomScan = agg_initialize_worker,
	.ShutdownCustomScan = agg_shutdown,
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

void
tess_agg_planner_init(void)
{
	previous_create_upper_paths_hook = create_upper_paths_hook;
	create_upper_paths_hook = create_upper_paths;
}

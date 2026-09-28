#include "postgres.h"

#include "access/parallel.h"
#include "catalog/pg_aggregate.h"
#include "catalog/pg_type_d.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "common/hashfn.h"
#include "common/int.h"
#include "executor/executor.h"
#include "lib/hyperloglog.h"
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
#include "utils/selfuncs.h"
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

/*
 * DISTINCT in an aggregate: a table of its own, without payload, keyed by
 * the group's keys and the argument, the argument alone without GROUP BY.
 * A row goes into the aggregate only when it inserted its pair: the ones
 * seen before are dropped, and so are NULL arguments, which the aggregate
 * skips anyway. The table does not spill.
 */
typedef struct DistinctSet
{
	MemoryContext context;
	int			nkeys;
	TessTableKeyKind kinds[TESS_TABLE_MAX_KEYS];
	TessTableRef table;
	void	  **bases;
	Size	   *lens;
	int			slots;
	Size		bytes;
	/* The buffers of a batch, for capacity rows. */
	int			capacity;
	uint32	   *hashes;
	uint32	   *offsets;
	uint64	   *pending_bits;
	uint64	   *inserted_bits;
	uint64	   *call_bits;
} DistinctSet;

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
	/* DISTINCT: the pairs of group and argument seen, and the argument's kind. */
	struct DistinctSet *distinct;
	TessTableKeyKind argument_kind;
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
	/*
	 * An aggregate has DISTINCT: its pairs of group and argument live in a
	 * table of their own, which does not spill, so neither do the groups.
	 */
	bool		has_distinct;
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

	/*
	 * Spilling: the level of partitions being read or given out, NULL
	 * while the groups fit; an index of the table's layout alone, for the
	 * files' fingerprint; how each aggregate's states merge; the counters.
	 */
	struct AggSpill *spill;
	void	   *layout_index;
	Size		layout_len;
	TessTableCombine *combines;
	uint64		partitions;
	uint64		evictions;
	uint64		spilled;
	uint64		disk_bytes;
	uint64		splits;
	/*
	 * Partial mode: the groups go out and the table starts anew whenever it
	 * would outgrow hash_mem, the Finalize Aggregate above merging a
	 * group's partials; the input may go on after a walk.
	 */
	bool		input_done;
	uint64		early_emits;
	/*
	 * The rows read when the table last started anew; and whether the
	 * groups go to disk instead, as a serial node's do, since a table sent
	 * up held nearly a group per row read: spread groups fold little
	 * before the table fills, and the Finalize Aggregate would get them
	 * all.
	 */
	uint64		emit_rows;
	bool		partial_spill;
} TessAggState;

static const CustomExecMethods agg_exec_methods;
static TessRowMask distinct_rows(TessAggState *state, AggValue *value, int nrows,
								 const uint32 *group_hashes,
								 const TessRowMask *valid,
								 const TessDatumColumn *argument);
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
		agg->aggfilter != NULL ||
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
	/* DISTINCT keys a table by the argument: an integer. */
	if (agg->aggdistinct != NIL)
		return exprType(argument) == INT4OID || exprType(argument) == INT8OID;
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
 * The expressions of grouping or distinct clauses when the node can group
 * by them: 1 to 16 values of a type the table keeps in a word
 * (tess_word_key_kind), a bare column, a chain the expression compiler
 * takes such as c % 10, or any other expression, computed row by row. NIL
 * otherwise, also when the planner dropped every grouping clause, as for
 * a constant one.
 */
static List *
clause_keys(PlannerInfo *root, List *clauses)
{
	List	   *keys = NIL;

	if (clauses == NIL || list_length(clauses) > TESS_TABLE_MAX_KEYS)
		return NIL;
	foreach_node(SortGroupClause, clause, clauses)
	{
		Node	   *expr = (Node *) get_sortgroupclause_expr(clause,
															root->processed_tlist);
		TessTableKeyKind kind;

		/* A key the compiler does not take is computed row by row. */
		if (!tess_word_key_kind(exprType(expr), &kind) || contain_subplans(expr) ||
			contain_volatile_functions(expr))
			return NIL;
		keys = lappend(keys, expr);
	}
	return keys;
}

static List *
group_keys(PlannerInfo *root)
{
	return clause_keys(root, root->processed_groupClause);
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
 * input, the grouping expressions and the aggregates it computes; the
 * groups spill past hash_mem as the core's do. NULL when the input cannot
 * be read in batches or lacks a column.
 */
static CustomPath *
make_agg_path(PlannerInfo *root, const AggPath *agg, List *tlist, int nkeys)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *child;
	Path		template;

	child = agg->subpath;
	/* A sort the core put below for its sorted grouping: hashing needs none. */
	while (IsA(child, SortPath) || IsA(child, IncrementalSortPath))
		child = ((SortPath *) child)->subpath;
	child = tess_batch_input_path(root, child);
	if (child == NULL || !arguments_available(tlist, child))
		return NULL;
	template = agg->path;
	template.total_cost *= AGG_COST_FACTOR;
	/* The groups come in no order, whatever order the core's had. */
	template.pathkeys = NIL;
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

/* Whether an aggregate of the target list has DISTINCT. */
static bool
has_distinct_aggregate(List *tlist)
{
	foreach_node(TargetEntry, entry, tlist)
		if (IsA(entry->expr, Aggref) && ((Aggref *) entry->expr)->aggdistinct != NIL)
			return true;
	return false;
}

/*
 * Whether the pairs of group and argument of the DISTINCT aggregates fit
 * hash_mem, as the planner estimates them: their tables do not spill. A
 * group key fewer than a table's most leaves room for the argument.
 */
static bool
distinct_fits(PlannerInfo *root, RelOptInfo *input_rel, List *keys, List *tlist)
{
	double		bytes = 0;

	foreach_node(TargetEntry, entry, tlist)
	{
		Aggref	   *agg;
		double		pairs;

		if (!IsA(entry->expr, Aggref) || ((Aggref *) entry->expr)->aggdistinct == NIL)
			continue;
		agg = (Aggref *) entry->expr;
		if (list_length(keys) >= TESS_TABLE_MAX_KEYS)
			return false;
		pairs = estimate_num_groups(root,
									lappend(list_copy(keys), aggregate_argument(agg)),
									input_rel->rows, NULL, NULL);
		bytes += pairs * (16.0 + 8.0 * (list_length(keys) + 1));
	}
	return bytes <= (double) get_hash_memory_limit();
}

/*
 * SELECT DISTINCT is grouping without aggregates: the node's path next to
 * each of the core's hashed distinct paths, over the same input, its keys
 * the distinct expressions. DISTINCT ON, which keeps other columns of a
 * row of each group, needs the order and stays with the core.
 */
static void
create_distinct_paths(PlannerInfo *root, RelOptInfo *input_rel,
					  RelOptInfo *output_rel)
{
	List	   *keys;
	List	   *tlist;

	if (root->parse->hasDistinctOn || IS_DUMMY_REL(input_rel))
		return;
	keys = clause_keys(root, root->processed_distinctClause);
	if (keys == NIL)
		return;
	tlist = add_to_flat_tlist(NIL, keys);
	/* add_path changes the list: the candidates are taken first. */
	foreach_ptr(AggPath, agg, aggregate_templates(output_rel->pathlist,
												  AGG_HASHED, AGGSPLIT_SIMPLE))
	{
		CustomPath *path;

		if (agg->groupClause == NIL ||
			not_from_groups((Node *) agg->path.pathtarget->exprs, keys))
			continue;
		path = make_agg_path(root, agg, tlist, list_length(keys));
		if (path != NULL)
			add_path(output_rel, &path->path);
	}
}

/* The relations of a set operation's leaves, the tree's whole in the top one. */
static Relids
setop_leaves(Node *node, Relids leaves)
{
	if (IsA(node, RangeTblRef))
		return bms_add_member(leaves, ((RangeTblRef *) node)->rtindex);
	leaves = setop_leaves(((SetOperationStmt *) node)->larg, leaves);
	return setop_leaves(((SetOperationStmt *) node)->rarg, leaves);
}

/*
 * UNION without ALL is grouping of the branches' rows by every column: the
 * node's path next to each of the core's hashed aggregate paths over the
 * Append of the branches, the node's Append below it. Only the set
 * operation of the whole query: above it the core puts only a sort and a
 * limit, which read columns by position, where the node's plan shows its
 * first branch's targets in place of the set operation's columns
 * (tess_plan_setop_columns); a set operation within another could have a
 * projection above that looks for the set operation's own.
 */
static void
create_setop_paths(PlannerInfo *root, RelOptInfo *output_rel)
{
	SetOperationStmt *top = (SetOperationStmt *) root->parse->setOperations;
	List	   *keys;
	List	   *tlist;

	if (top == NULL || IS_DUMMY_REL(output_rel) ||
		!bms_equal(output_rel->relids, setop_leaves((Node *) top, NULL)))
		return;
	/* add_path changes the list: the candidates are taken first. */
	foreach_ptr(AggPath, agg, aggregate_templates(output_rel->pathlist,
												  AGG_HASHED, AGGSPLIT_SIMPLE))
	{
		CustomPath *path;

		keys = agg->path.pathtarget->exprs;
		if (!IsA(agg->subpath, AppendPath) || keys == NIL ||
			list_length(keys) > TESS_TABLE_MAX_KEYS ||
			list_length(agg->groupClause) != list_length(keys))
			continue;
		foreach_ptr(Node, key, keys)
		{
			TessTableKeyKind kind;

			if (!tess_word_key_kind(exprType(key), &kind))
			{
				keys = NIL;
				break;
			}
		}
		if (keys == NIL)
			continue;
		tlist = add_to_flat_tlist(NIL, keys);
		path = make_agg_path(root, agg, tlist, list_length(keys));
		if (path != NULL)
			add_path(output_rel, &path->path);
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
	List	   *templates;
	AggStrategy strategy = AGG_PLAIN;
	double		groups = 0;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel,
										 extra);
	if (!*tess_runtime_api()->settings->enable)
		return;
	if (stage == UPPERREL_DISTINCT)
	{
		create_distinct_paths(root, input_rel, output_rel);
		return;
	}
	if (stage == UPPERREL_SETOP)
	{
		create_setop_paths(root, output_rel);
		return;
	}
	if (stage != UPPERREL_GROUP_AGG ||
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
	if (!distinct_fits(root, input_rel, keys, tlist))
		return;
	/*
	 * add_path changes the list: the candidates are taken first. With
	 * DISTINCT in an aggregate the core groups only sorted; the node hashes
	 * in its place, unless hashing is disabled.
	 */
	templates = aggregate_templates(output_rel->pathlist, strategy, AGGSPLIT_SIMPLE);
	if (templates == NIL && strategy == AGG_HASHED && enable_hashagg &&
		has_distinct_aggregate(tlist))
		templates = aggregate_templates(output_rel->pathlist, AGG_SORTED,
										AGGSPLIT_SIMPLE);
	foreach_ptr(AggPath, agg, templates)
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
	/* Over a set operation's rows: its columns are the child's targets. */
	tlist = (List *) tess_plan_setop_columns((Node *) tlist, child.plan);
	info.expressions = (List *) tess_plan_setop_columns((Node *) info.expressions, child.plan);
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

		if (!tess_word_key_kind(exprType(key), &state->kinds[position]))
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
			if (agg->aggdistinct != NIL)
			{
				value->argument_kind = exprType(argument) == INT8OID ?
					TESS_TABLE_KEY_INT8 : TESS_TABLE_KEY_INT4;
				value->distinct = palloc0(sizeof(struct DistinctSet));
				state->has_distinct = true;
			}
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
			!TESS_ABI_HAS_FIELD(state->kernels, TessKernelOps, table_combine))
			elog(ERROR, "TessAgg needs the kernels module for GROUP BY");
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
									   sizeof(uint64) * (1 + state->nvalues),
									   AGG_INITIAL_GROUPS, &state->layout_len,
									   &state->status) != TESS_OK)
			tess_status_report(&state->status);
		state->layout_index = palloc0(state->layout_len);
		if (state->kernels->table_create(state->layout_index, state->layout_len,
										 state->nkeys, state->kinds,
										 sizeof(uint64) * (1 + state->nvalues),
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
		if (state->kernels == NULL ||
			!TESS_ABI_HAS_FIELD(state->kernels, TessKernelOps, table_combine))
			elog(ERROR, "TessAgg needs the kernels module for DISTINCT");
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
	if (value->distinct != NULL)
	{
		TessRowMask rows = distinct_rows(state, value, batch->rows.nrows, NULL,
										 &batch->rows, column);

		evaluate(state, value, column, &rows);
		return;
	}
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

static void distinct_reset(TessAggState *state, AggValue *value);

/* Empty every distinct set, before the input is read. */
static void
reset_distinct(TessAggState *state)
{
	for (int index = 0; index < state->nvalues; index++)
		if (state->values[index].distinct != NULL)
			distinct_reset(state, &state->values[index]);
}

/* Read every batch of the child into the running values. */
static void
drain(TessAggState *state)
{
	reset_distinct(state);
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

static Size agg_spill_memory(TessAggState *state);
static Size distinct_bytes(TessAggState *state);

/*
 * The bytes of the table now, and the most so far; once it spills, the
 * index and every chunk of every level, which live in the levels' memory.
 */
static void
note_memory(TessAggState *state)
{
	Size		memory = state->spill == NULL ? state->table_bytes :
		agg_spill_memory(state);

	if (state->has_distinct)
		memory += distinct_bytes(state);
	state->peak_memory = Max(state->peak_memory, memory);
}

/*
 * The groups' records lie in chunks: the first of AGG_FIRST_CHUNK bytes,
 * so that a few groups take little, the others of the most a chunk may
 * have. Records never move; when they reach half the buckets, only the
 * index is made anew, larger.
 */
#define AGG_FIRST_CHUNK (64 * 1024)

/*
 * A first index for capacity groups at most, and at most a quarter of
 * hash_mem, about 8 bytes of buckets per group: an estimate too large
 * would take the memory the groups need; the index grows as they come.
 */
static uint64
first_capacity(uint64 capacity)
{
	uint64		most = get_hash_memory_limit() / 32;

	return Max(Min(capacity, most), AGG_INITIAL_GROUPS);
}

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
	uint64		capacity = first_capacity(state->groups_estimate);
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
	/* Past the first, an eighth of hash_mem, so that a small one spills late. */
	Size		len = chunk == 0 ? AGG_FIRST_CHUNK :
		Max(AGG_FIRST_CHUNK, Min(TESS_TABLE_MAX_CHUNK_LEN,
								 TYPEALIGN_DOWN(8, get_hash_memory_limit() / 8)));
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
	void	   *index;

	/* find_or_insert stopped at half the buckets, over existing chunks. */
	Assert(groups > 0 && state->table.nchunks > 0);
	index = new_index(state, groups * 2, &size);

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

/* An index of the pairs for capacity of them in the set's memory. */
static void *
distinct_index(TessAggState *state, DistinctSet *set, uint64 capacity, Size *size)
{
	check(state, state->kernels->table_size(set->nkeys, set->kinds, 0, capacity,
											size, &state->status));
	return MemoryContextAllocExtended(set->context, *size, MCXT_ALLOC_HUGE);
}

/* An empty set: the groups' keys, then the argument. */
static void
distinct_reset(TessAggState *state, AggValue *value)
{
	DistinctSet *set = value->distinct;
	uint64		capacity = AGG_INITIAL_GROUPS;
	Size		size;

	if (set->context == NULL)
		set->context = AllocSetContextCreate(state->css.ss.ps.state->es_query_cxt,
											 "TessAgg distinct",
											 ALLOCSET_DEFAULT_SIZES);
	MemoryContextReset(set->context);
	set->nkeys = state->nkeys + 1;
	for (int key = 0; key < state->nkeys; key++)
		set->kinds[key] = state->kinds[key];
	set->kinds[state->nkeys] = value->argument_kind;
	set->capacity = 0;
	set->slots = 16;
	set->bases = MemoryContextAlloc(set->context, sizeof(void *) * set->slots);
	set->lens = MemoryContextAlloc(set->context, sizeof(Size) * set->slots);
	set->table.index = distinct_index(state, set, capacity, &size);
	set->table.index_len = size;
	set->table.chunks = set->bases;
	set->table.chunk_lens = set->lens;
	set->table.nchunks = 0;
	set->bytes = size;
	check(state, state->kernels->table_create(set->table.index, size, set->nkeys,
											  set->kinds, 0, capacity,
											  &state->status));
}

static void
distinct_add_chunk(TessAggState *state, DistinctSet *set)
{
	int			chunk = set->table.nchunks;
	Size		len = chunk == 0 ? AGG_FIRST_CHUNK : TESS_TABLE_MAX_CHUNK_LEN;
	void	   *base;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessAgg distinct table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	if (chunk == set->slots)
	{
		set->slots *= 2;
		set->bases = repalloc(set->bases, sizeof(void *) * set->slots);
		set->lens = repalloc(set->lens, sizeof(Size) * set->slots);
		set->table.chunks = set->bases;
		set->table.chunk_lens = set->lens;
	}
	base = MemoryContextAlloc(set->context, len);
	check(state, state->kernels->table_chunk_init(base, len, &state->status));
	set->bases[chunk] = base;
	set->lens[chunk] = len;
	set->table.nchunks++;
	set->bytes += len;
}

static void
distinct_regrow(TessAggState *state, DistinctSet *set, uint64 records)
{
	void	   *old = set->table.index;
	Size		size;
	void	   *index = distinct_index(state, set, records * 2, &size);

	check(state, state->kernels->table_regrow(&set->table, index, size,
											  records * 2, &state->status));
	set->bytes = set->bytes - set->table.index_len + size;
	set->table.index = index;
	set->table.index_len = size;
	pfree(old);
}

/*
 * The rows of valid, of a batch of nrows rows, that insert their pair into
 * the aggregate's set: a mask in the set's buffers. group_hashes are the
 * rows' hashes of the groups' keys, state->table_keys their keys, or NULL
 * without GROUP BY.
 */
static TessRowMask
distinct_rows(TessAggState *state, AggValue *value, int nrows,
			  const uint32 *group_hashes, const TessRowMask *valid,
			  const TessDatumColumn *argument)
{
	DistinctSet *set = value->distinct;
	int			nwords = tess_row_mask_word_count(nrows);
	TessTableKey keys[TESS_TABLE_MAX_KEYS];
	TessRowMask pending;
	TessRowMask inserted;
	bool		int8 = value->argument_kind == TESS_TABLE_KEY_INT8;

	if (set->capacity < nrows)
	{
		set->hashes = MemoryContextAlloc(set->context, sizeof(uint32) * nrows);
		set->offsets = MemoryContextAlloc(set->context, sizeof(uint32) * nrows);
		set->pending_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
		set->inserted_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
		set->call_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
		set->capacity = nrows;
	}
	pending = (TessRowMask) {nrows, set->pending_bits};
	inserted = (TessRowMask) {nrows, set->inserted_bits};
	memset(set->inserted_bits, 0, sizeof(uint64) * nwords);
	/* The groups' hashes folded with the argument's; a NULL one drops out. */
	if (group_hashes != NULL)
	{
		memcpy(set->hashes, group_hashes, sizeof(uint32) * nrows);
		memcpy(set->pending_bits, valid->bits, sizeof(uint64) * nwords);
		check(state, (int8 ? state->kernels->int8_hash_next :
					  state->kernels->int4_hash_next) (argument, NULL,
													   TESS_NULL_KEYS_REJECT,
													   set->hashes, &pending,
													   &state->status));
	}
	else
	{
		memset(set->pending_bits, 0, sizeof(uint64) * nwords);
		check(state, (int8 ? state->kernels->int8_hash :
					  state->kernels->int4_hash) (argument, NULL, valid,
												  TESS_NULL_KEYS_REJECT,
												  set->hashes, &pending,
												  &state->status));
	}
	for (int key = 0; key < state->nkeys; key++)
		keys[key] = state->table_keys[key];
	keys[state->nkeys].kind = value->argument_kind;
	keys[state->nkeys].column = argument;
	keys[state->nkeys].prepared = NULL;
	if (set->table.nchunks == 0)
		distinct_add_chunk(state, set);
	for (;;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
		TessRowMask call = {nrows, set->call_bits};

		/*
		 * Each call fills its mask of new pairs whole: they add up. A mask
		 * has no bits past its rows on entry, which a longer batch left.
		 */
		memset(set->call_bits, 0, sizeof(uint64) * nwords);
		check(state, state->kernels->table_find_or_insert(&set->table,
														  set->table.nchunks - 1,
														  set->hashes, set->nkeys,
														  keys, &pending,
														  set->offsets, &call,
														  &state->status));
		for (int word = 0; word < nwords; word++)
			set->inserted_bits[word] |= set->call_bits[word];
		if (tess_row_mask_count(&pending) == 0)
			break;
		check(state, state->kernels->table_stats(&set->table, &stats,
												 &state->status));
		if (stats.records * 2 >= stats.buckets)
			distinct_regrow(state, set, stats.records);
		else
			distinct_add_chunk(state, set);
	}
	return inserted;
}

/* The bytes of every distinct set. */
static Size
distinct_bytes(TessAggState *state)
{
	Size		bytes = 0;

	for (int index = 0; index < state->nvalues; index++)
		if (state->values[index].distinct != NULL)
			bytes += state->values[index].distinct->bytes;
	return bytes;
}

/*
 * Spilling (plan item 5.6, docs/spill.md). The groups fit until the table
 * takes more than hash_mem; then they go into partitions by the hashes'
 * low bits under the one index, and while the table takes more, the
 * largest partition goes to disk whole: its records, each a group's
 * states, are written and freed, and the index is made anew over the
 * rest. Its rows then make new records, which are merged with those on
 * disk once the input is done: partition by partition, the records in
 * memory make a table, and the chunks read back merge into it
 * (tess_table_combine). A partition too large to merge is split first by
 * the next bits of the hash into a level of its own.
 *
 * The chunk arrays keep two slots first: AGG_SOURCE, empty or a chunk read
 * back for a merge or a split, and AGG_EMPTY, an empty chunk a partition
 * without one appends to, which sends its rows back for a chunk.
 */
#define AGG_SPILL_MIN_CHUNK (8 * 1024)
#define AGG_SPILL_MIN_PARTITIONS 4
#define AGG_SPILL_MAX_PARTITIONS 1024
#define AGG_SOURCE 0
#define AGG_EMPTY 1

/* A partition: its chunks in memory, the last the one it appends to. */
typedef struct AggPart
{
	void	  **chunks;
	int			nchunks;
	int			slots;
	Size		bytes;
	/* Records in memory and on disk, and the bytes written. */
	uint64		records;
	uint64		disk_records;
	uint64		disk_bytes;
	/*
	 * The partition's groups, estimated from the hashes of every record
	 * made in it (HyperLogLog, as the core's hash aggregate keeps one per
	 * spilled partition): what a merge holds, however many times a group
	 * went to disk.
	 */
	hyperLogLogState groups;
} AggPart;

/* Registers of a partition's estimate, 2^6 bytes: an error of about 13 %. */
#define AGG_GROUPS_WIDTH 6

typedef struct AggSpill
{
	struct AggSpill *parent;
	/* The chunks, in blocks of their own size; the blocks read back. */
	MemoryContext context;
	MemoryContext block_context;
	uint32		level;
	uint32		shift;
	int			npartitions;
	Size		chunk_len;
	AggPart    *parts;
	/*
	 * A split's chunks: the source, the empty one, and each partition's
	 * current chunk at AGG_EMPTY + 1 + partition, or the empty one.
	 */
	void	  **bases;
	Size	   *lens;
	uint32	   *current;
	TessSpill  *file;
	uint32		next_number;
	/* The input is read; the partition being given out, -1 before any. */
	bool		done_input;
	int			partition;
	/*
	 * Giving out: the partitions wholly in memory first, which merge with
	 * nothing and free their memory, then those with records on disk (pass
	 * 1); whether the current partition was given out, to free it next.
	 */
	int			pass;
	bool		given;
	/* The empty chunks of the two first slots. */
	uint64		source_empty[1];
	uint64		empty[1];
} AggSpill;

static inline uint32
agg_partition(const AggSpill *spill, uint32 hash)
{
	return (hash >> spill->shift) & (uint32) (spill->npartitions - 1);
}

static inline Size
agg_chunk_used(const void *base)
{
	return (Size) *(const uint64 *) base;
}

/* The bytes of a record: header, key slots, flags and a word per aggregate. */
static Size
agg_record_size(TessAggState *state)
{
	return 16 + 8 * state->nkeys + sizeof(uint64) * (1 + state->nvalues);
}

static void
part_push(AggSpill *spill, AggPart *part, void *base)
{
	if (part->nchunks == part->slots)
	{
		part->slots = Max(part->slots * 2, 4);
		part->chunks = part->chunks == NULL ?
			MemoryContextAlloc(spill->context, sizeof(void *) * part->slots) :
			repalloc(part->chunks, sizeof(void *) * part->slots);
	}
	part->chunks[part->nchunks++] = base;
	part->bytes += spill->chunk_len;
}

static void *
agg_new_chunk(TessAggState *state, AggSpill *spill)
{
	void	   *base = MemoryContextAlloc(spill->context, spill->chunk_len);

	check(state, state->kernels->table_chunk_init(base, spill->chunk_len,
												  &state->status));
	return base;
}

/* Free a partition's chunks in memory. */
static void
part_release(AggSpill *spill, AggPart *part)
{
	for (int chunk = 0; chunk < part->nchunks; chunk++)
		pfree(part->chunks[chunk]);
	part->nchunks = 0;
	part->bytes = 0;
	part->records = 0;
}

/* Write a chunk of the partition's records, unless it holds none. */
static void
agg_write_chunk(TessAggState *state, AggSpill *spill, int partition, void *base)
{
	Size		used = agg_chunk_used(base);

	if (used <= TESS_TABLE_CHUNK_HEADER)
		return;
	/* disk_bytes of a partition: what it takes read back; the node's, what was stored. */
	state->disk_bytes += tess_spill_write(spill->file, partition, TESS_SPILL_RECORDS,
										  spill->next_number++, base, used, NULL);
	spill->parts[partition].disk_bytes += used;
	spill->parts[partition].disk_records +=
		(used - TESS_TABLE_CHUNK_HEADER) / agg_record_size(state);
	state->spilled++;
}

/*
 * A level of partitions by the hash bits from shift, for expected bytes:
 * the power of two that makes each about half of hash_mem, as long as the
 * bits last and a chunk per partition fits in half of hash_mem.
 */
static AggSpill *
agg_spill_create(TessAggState *state, AggSpill *parent, double expected, uint32 shift)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	Size		limit = get_hash_memory_limit();
	Size		record = agg_record_size(state);
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	TessTableRef layout = {0};
	AggSpill   *spill = MemoryContextAllocZero(context, sizeof(AggSpill));
	int			npartitions = AGG_SPILL_MIN_PARTITIONS;
	Size		chunk_len;

	/* Each partition keeps a chunk and its file's buffer of a page. */
	while (npartitions < AGG_SPILL_MAX_PARTITIONS &&
		   (double) npartitions * (limit / 2) < expected &&
		   (Size) npartitions * 2 * (AGG_SPILL_MIN_CHUNK + BLCKSZ) <= limit / 2 &&
		   shift + pg_leftmost_one_pos32(npartitions) + 1 < 32)
		npartitions *= 2;
	chunk_len = limit / (8 * npartitions);
	chunk_len = Min(chunk_len, TESS_TABLE_MAX_CHUNK_LEN);
	chunk_len = Max(chunk_len, AGG_SPILL_MIN_CHUNK);
	chunk_len = Max(chunk_len, TESS_TABLE_CHUNK_HEADER + 4 * record);
	spill->chunk_len = TYPEALIGN_DOWN(8, chunk_len);
	spill->parent = parent;
	spill->level = parent == NULL ? 0 : parent->level + 1;
	spill->shift = shift;
	spill->npartitions = npartitions;
	spill->partition = -1;
	/* Small blocks: a chunk takes a block of its own size. */
	spill->context = AllocSetContextCreate(context, "TessAgg spill",
										   ALLOCSET_SMALL_SIZES);
	spill->block_context = AllocSetContextCreate(spill->context,
												 "TessAgg spilled block",
												 ALLOCSET_SMALL_SIZES);
	spill->parts = MemoryContextAllocZero(spill->context,
										  sizeof(AggPart) * npartitions);
	{
		MemoryContext old = MemoryContextSwitchTo(spill->context);

		for (int partition = 0; partition < npartitions; partition++)
			initHyperLogLog(&spill->parts[partition].groups, AGG_GROUPS_WIDTH);
		MemoryContextSwitchTo(old);
	}
	spill->bases = MemoryContextAlloc(spill->context,
									  sizeof(void *) * (npartitions + 2));
	spill->lens = MemoryContextAlloc(spill->context, sizeof(Size) * (npartitions + 2));
	spill->current = MemoryContextAlloc(spill->context, sizeof(uint32) * npartitions);
	spill->source_empty[0] = TESS_TABLE_CHUNK_HEADER;
	spill->empty[0] = TESS_TABLE_CHUNK_HEADER;
	spill->bases[AGG_SOURCE] = spill->source_empty;
	spill->lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
	for (int partition = 0; partition < npartitions; partition++)
	{
		spill->bases[AGG_EMPTY + 1 + partition] = spill->empty;
		spill->lens[AGG_EMPTY + 1 + partition] = TESS_TABLE_CHUNK_HEADER;
		spill->current[partition] = AGG_EMPTY + 1 + partition;
	}
	spill->bases[AGG_EMPTY] = spill->empty;
	spill->lens[AGG_EMPTY] = TESS_TABLE_CHUNK_HEADER;
	/* The files' fingerprint: the table's layout, from the index. */
	layout.index = state->layout_index;
	layout.index_len = state->layout_len;
	layout.chunks = spill->bases;
	layout.chunk_lens = spill->lens;
	check(state, state->kernels->table_fingerprint(&layout, &config.fingerprint,
												   &state->status));
	config.parent_context = spill->context;
	config.kernels = state->kernels;
	config.npartitions = npartitions;
	config.level = spill->level;
	config.max_len = (uint64) MaxAllocHugeSize;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(get_hash_memory_limit());
	spill->file = tess_spill_create(&config);
	state->partitions = Max(state->partitions, (uint64) npartitions);
	return spill;
}

/* Delete a level's files and free its memory. */
static void
agg_level_free(AggSpill *spill)
{
	tess_spill_free(spill->file);
	MemoryContextDelete(spill->context);
	pfree(spill);
}

static void
agg_spill_free(TessAggState *state)
{
	while (state->spill != NULL)
	{
		AggSpill   *parent = state->spill->parent;

		agg_level_free(state->spill);
		state->spill = parent;
	}
}

/*
 * Split a chunk of records, placed at the source slot, into the level's
 * partitions by the kernel: a partition whose chunk fills keeps it and
 * gets another with keep, or writes it and starts it again otherwise.
 */
static void
agg_split(TessAggState *state, AggSpill *spill, void *base, Size len, bool keep)
{
	TessTableRef ref = {0};
	uint32		offsets[AGG_GROUP_ROWS];
	uint32		hashes[AGG_GROUP_ROWS];
	Size		from = TESS_TABLE_CHUNK_HEADER;

	spill->bases[AGG_SOURCE] = base;
	spill->lens[AGG_SOURCE] = len;
	ref.chunks = spill->bases;
	ref.chunk_lens = spill->lens;
	ref.nchunks = spill->npartitions + 2;
	for (;;)
	{
		int			count;
		int			full;

		check(state, state->kernels->table_split(&ref, state->nkeys, state->kinds,
												 sizeof(uint64) * (1 + state->nvalues),
												 spill->current, spill->npartitions,
												 spill->shift, AGG_SOURCE, &from,
												 AGG_GROUP_ROWS, offsets, hashes,
												 &count, &full, &state->status));
		for (int index = 0; index < count; index++)
		{
			AggPart    *part = &spill->parts[agg_partition(spill, hashes[index])];

			part->records++;
			addHyperLogLog(&part->groups, murmurhash32(hashes[index]));
		}
		if (full >= 0)
		{
			AggPart    *part = &spill->parts[full];
			int			slot = AGG_EMPTY + 1 + full;

			if (spill->bases[slot] == spill->empty)
			{
				void	   *chunk = agg_new_chunk(state, spill);

				part_push(spill, part, chunk);
				spill->bases[slot] = chunk;
				spill->lens[slot] = spill->chunk_len;
			}
			else if (keep)
			{
				void	   *chunk = agg_new_chunk(state, spill);

				part_push(spill, part, chunk);
				spill->bases[slot] = chunk;
			}
			else
			{
				agg_write_chunk(state, spill, full, spill->bases[slot]);
				part->records = 0;
				check(state, state->kernels->table_chunk_init(spill->bases[slot],
															  spill->chunk_len,
															  &state->status));
			}
		}
		else if (count == 0)
			break;
	}
	spill->bases[AGG_SOURCE] = spill->source_empty;
	spill->lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
}

/*
 * The table over the partitions' chunks in memory: the chunk arrays made
 * anew, each partition appending to its last chunk, and an index for
 * twice their records, into which every record is linked.
 */
static void
agg_table_from_parts(TessAggState *state, AggSpill *spill, uint64 capacity)
{
	Size		payload_size = sizeof(uint64) * (1 + state->nvalues);
	int			nchunks = 2;
	Size		size;
	void	   *old = state->table.index;

	for (int partition = 0; partition < spill->npartitions; partition++)
		nchunks += spill->parts[partition].nchunks;
	if (nchunks > state->chunk_slots)
	{
		state->chunk_slots = Max(nchunks, state->chunk_slots * 2);
		state->chunk_bases = repalloc(state->chunk_bases,
									  sizeof(void *) * state->chunk_slots);
		state->chunk_lens = repalloc(state->chunk_lens,
									 sizeof(Size) * state->chunk_slots);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
	state->chunk_bases[AGG_EMPTY] = spill->empty;
	state->chunk_lens[AGG_EMPTY] = TESS_TABLE_CHUNK_HEADER;
	nchunks = 2;
	state->table_bytes = 0;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		AggPart    *part = &spill->parts[partition];

		spill->current[partition] = AGG_EMPTY;
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			state->chunk_bases[nchunks] = part->chunks[chunk];
			state->chunk_lens[nchunks] = spill->chunk_len;
			spill->current[partition] = nchunks++;
			state->table_bytes += spill->chunk_len;
		}
	}
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
	state->table.nchunks = nchunks;
	capacity = first_capacity(capacity);
	state->table.index = new_index(state, capacity, &size);
	state->table.index_len = size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys,
											  state->kinds, payload_size,
											  capacity, &state->status));
	for (int chunk = AGG_EMPTY + 1; chunk < nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;

		check(state, state->kernels->table_link(&state->table, chunk, &from, NULL,
												NULL, &state->status));
	}
	if (old != NULL)
		pfree(old);
	state->table_bytes += size;
	note_memory(state);
}

static bool agg_evict(TessAggState *state, Size extra);
static uint64 agg_records(AggSpill *spill);

/*
 * The table outgrew hash_mem: the first level of partitions, for twice the
 * groups so far or the planner's if more, and the records so far split
 * into them.
 */
static void
agg_start_spill(TessAggState *state)
{
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	double		bytes = state->table_bytes;
	double		expected;
	AggSpill   *spill;
	int			nold = state->table.nchunks;
	void	  **old = palloc(sizeof(void *) * Max(nold, 1));
	Size	   *old_lens = palloc(sizeof(Size) * Max(nold, 1));

	check(state, state->kernels->table_stats(&state->table, &stats, &state->status));
	expected = bytes * 2;
	if (stats.records > 0)
		expected = Max(expected,
					   bytes / stats.records * (double) state->groups_estimate);
	spill = agg_spill_create(state, NULL, expected, 0);
	state->spill = spill;
	memcpy(old, state->chunk_bases, sizeof(void *) * nold);
	memcpy(old_lens, state->chunk_lens, sizeof(Size) * nold);
	for (int chunk = 0; chunk < nold; chunk++)
	{
		agg_split(state, spill, old[chunk], old_lens[chunk], true);
		pfree(old[chunk]);
	}
	pfree(old);
	pfree(old_lens);
	/* Room for the index first: the old one goes, the new one is made last. */
	pfree(state->table.index);
	state->table.index = NULL;
	(void) agg_evict(state, sizeof(uint64) * first_capacity(agg_records(spill) * 2));
	agg_table_from_parts(state, spill, agg_records(spill) * 2);
}

/*
 * Once the table, with its files' buffers, takes more than seven eighths
 * of hash_mem, the partition with the most bytes in memory goes to disk
 * whole, and the next, until the table takes half of hash_mem; the index
 * is then made anew over the rest. Evicting down to the limit only made
 * the index anew after every partition: 5 M groups of a row each at a
 * work_mem of 4 MB made it 3598 times.
 */
static bool
agg_evict(TessAggState *state, Size extra)
{
	AggSpill   *spill = state->spill;
	/* An eighth of hash_mem is left for a batch's new chunks and index. */
	Size		limit = get_hash_memory_limit() / 8 * 7;
	Size		target = get_hash_memory_limit() / 2;
	bool		evicted = false;

	if (agg_spill_memory(state) + extra <= limit)
		return false;
	while (agg_spill_memory(state) + extra > target)
	{
		int			largest = -1;
		Size		bytes = 0;

		for (int partition = 0; partition < spill->npartitions; partition++)
			if (spill->parts[partition].bytes > bytes)
			{
				largest = partition;
				bytes = spill->parts[partition].bytes;
			}
		if (largest < 0)
			break;
		for (int chunk = 0; chunk < spill->parts[largest].nchunks; chunk++)
			agg_write_chunk(state, spill, largest, spill->parts[largest].chunks[chunk]);
		part_release(spill, &spill->parts[largest]);
		state->evictions++;
		evicted = true;
	}
	return evicted;
}

/* The records of the partitions in memory, for their index. */
static uint64
agg_records(AggSpill *spill)
{
	uint64		records = 0;

	for (int partition = 0; partition < spill->npartitions; partition++)
		records += spill->parts[partition].records;
	return records;
}

static void
agg_make_room(TessAggState *state)
{
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	Size		extra = 0;

	/*
	 * An index a batch could fill grows by a new one twice its size next
	 * to it: counted now, so that the partitions go to disk before the
	 * table outgrows hash_mem in the middle of a batch.
	 */
	check(state, state->kernels->table_stats(&state->table, &stats, &state->status));
	if ((stats.records + state->capacity) * 2 >= stats.buckets)
		extra = 2 * state->table.index_len;
	if (agg_evict(state, extra))
		agg_table_from_parts(state, state->spill, agg_records(state->spill) * 2);
}

/*
 * The rows of a batch into the groups of a table that spills: new groups
 * go to their partitions' chunks, a partition without room getting
 * another; a full index grows. Returns with every row resolved.
 */
static void
agg_find_partitioned(TessAggState *state, TessRowMask *pending, TessRowMask *inserted)
{
	AggSpill   *spill = state->spill;
	int			nrows = pending->nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	uint64	   *found = palloc0(sizeof(uint64) * nwords);
	bool	   *seen = palloc(sizeof(bool) * spill->npartitions);

	for (;;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
		TessRowMask created = {nrows, found};
		int			row = -1;
		bool		index_full;

		memset(found, 0, sizeof(uint64) * nwords);
		check(state, state->kernels->table_find_or_insert_partitioned(&state->table,
																	  spill->current,
																	  spill->npartitions,
																	  spill->shift,
																	  state->hashes,
																	  state->nkeys,
																	  state->table_keys,
																	  pending,
																	  state->offsets,
																	  &created,
																	  &state->status));
		while ((row = tess_row_mask_next(&created, row)) >= 0)
		{
			AggPart    *part = &spill->parts[agg_partition(spill, state->hashes[row])];

			part->records++;
			/* The partition's bits are the hash's low ones: mixed first. */
			addHyperLogLog(&part->groups, murmurhash32(state->hashes[row]));
		}
		for (int word = 0; word < nwords; word++)
			inserted->bits[word] |= found[word];
		if (tess_row_mask_count(pending) == 0)
			break;
		check(state, state->kernels->table_stats(&state->table, &stats,
												 &state->status));
		index_full = stats.records * 2 >= stats.buckets;
		if (index_full)
		{
			regrow_table(state, stats.records);
			continue;
		}
		/*
		 * A new chunk for each partition that has rows left: its chunk ran
		 * out of room, or it had none.
		 */
		memset(seen, 0, sizeof(bool) * spill->npartitions);
		row = -1;
		while ((row = tess_row_mask_next(pending, row)) >= 0)
		{
			int			partition = agg_partition(spill, state->hashes[row]);
			AggPart    *part = &spill->parts[partition];
			int			chunk = state->table.nchunks;

			if (seen[partition])
				continue;
			seen[partition] = true;
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
			if (chunk == TESS_TABLE_MAX_CHUNKS)
				ereport(ERROR,
						(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
						 errmsg("TessAgg group table cannot hold more than %d chunks",
								TESS_TABLE_MAX_CHUNKS)));
			state->chunk_bases[chunk] = agg_new_chunk(state, spill);
			state->chunk_lens[chunk] = spill->chunk_len;
			part_push(spill, part, state->chunk_bases[chunk]);
			spill->current[partition] = chunk;
			state->table.nchunks++;
			state->table_bytes += spill->chunk_len;
		}
		note_memory(state);
	}
	pfree(found);
	pfree(seen);
}

/*
 * Merge a chunk of groups' states, placed at the source slot, into the
 * partition's table by the kernel: a group the table lacks goes to the
 * chunk at *dest, or to a new one of the partition's when that is full.
 */
static void
agg_combine(TessAggState *state, AggSpill *spill, AggPart *part, void *base,
			Size len, int *dest)
{
	Size		from = TESS_TABLE_CHUNK_HEADER;

	state->chunk_bases[AGG_SOURCE] = base;
	state->chunk_lens[AGG_SOURCE] = len;
	for (;;)
	{
		int			merged;
		int			stop;

		/* A partition with nothing in memory takes a chunk for its groups. */
		if (*dest <= AGG_EMPTY)
			stop = TESS_TABLE_COMBINE_CHUNK_FULL;
		else
			check(state, state->kernels->table_combine(&state->table, AGG_SOURCE,
													   &from, *dest, state->nvalues,
													   state->combines, &merged,
													   &stop, &state->status));
		if (stop == TESS_TABLE_COMBINE_DONE)
			break;
		if (stop == TESS_TABLE_COMBINE_INDEX_FULL)
		{
			TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

			/*
			 * A new index links every chunk's records: the source's, merged
			 * or not, are no groups of the table. Hidden, or a group whose
			 * record the source still holds would be found there and never
			 * merged into its record of the table.
			 */
			state->chunk_bases[AGG_SOURCE] = spill->source_empty;
			state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
			check(state, state->kernels->table_stats(&state->table, &stats,
													 &state->status));
			regrow_table(state, stats.records);
			state->chunk_bases[AGG_SOURCE] = base;
			state->chunk_lens[AGG_SOURCE] = len;
			continue;
		}
		if (state->table.nchunks == state->chunk_slots)
		{
			state->chunk_slots *= 2;
			state->chunk_bases = repalloc(state->chunk_bases,
										  sizeof(void *) * state->chunk_slots);
			state->chunk_lens = repalloc(state->chunk_lens,
										 sizeof(Size) * state->chunk_slots);
			state->table.chunks = state->chunk_bases;
			state->table.chunk_lens = state->chunk_lens;
		}
		*dest = state->table.nchunks++;
		state->chunk_bases[*dest] = agg_new_chunk(state, spill);
		state->chunk_lens[*dest] = spill->chunk_len;
		part_push(spill, part, state->chunk_bases[*dest]);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
}

/*
 * The groups a partition merges into: its estimate with a third more for
 * the estimate's error, at most its records.
 */
static uint64
part_groups(AggPart *part)
{
	double		groups = estimateHyperLogLog(&part->groups) * 4 / 3;

	return (uint64) Min(groups, (double) (part->records + part->disk_records));
}

/*
 * The table of one partition, with an index for its records in memory and
 * on disk: its chunks in memory linked, when they hold each group once, as
 * those the first level found by the index do, and merged by the kernel
 * otherwise, as a level below's split them; then its chunks read back
 * merged in, a group the table lacks copied to its last chunk or a new one.
 */
static void
agg_merge(TessAggState *state, AggSpill *spill, int partition)
{
	AggPart    *part = &spill->parts[partition];
	Size		payload_size = sizeof(uint64) * (1 + state->nvalues);
	uint64		capacity = first_capacity(part_groups(part));
	bool		unique = spill->parent == NULL;
	void	  **split = NULL;
	int			nsplit = 0;
	TessSpillReader *reader;
	TessSpillHeader header;
	int			nchunks = 2;
	Size		size;
	int			dest;

	/* A level below's chunks are merged as sources, the partition's afresh. */
	if (!unique && part->nchunks > 0)
	{
		nsplit = part->nchunks;
		split = palloc(sizeof(void *) * nsplit);
		memcpy(split, part->chunks, sizeof(void *) * nsplit);
		part->nchunks = 0;
		part->bytes = 0;
	}

	if (state->table.index != NULL)
		pfree(state->table.index);
	if (part->nchunks + 3 > state->chunk_slots)
	{
		state->chunk_slots = Max(part->nchunks + 3, state->chunk_slots * 2);
		state->chunk_bases = repalloc(state->chunk_bases,
									  sizeof(void *) * state->chunk_slots);
		state->chunk_lens = repalloc(state->chunk_lens,
									 sizeof(Size) * state->chunk_slots);
	}
	state->chunk_bases[AGG_SOURCE] = spill->source_empty;
	state->chunk_lens[AGG_SOURCE] = TESS_TABLE_CHUNK_HEADER;
	state->chunk_bases[AGG_EMPTY] = spill->empty;
	state->chunk_lens[AGG_EMPTY] = TESS_TABLE_CHUNK_HEADER;
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		state->chunk_bases[nchunks] = part->chunks[chunk];
		state->chunk_lens[nchunks++] = spill->chunk_len;
	}
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
	state->table.nchunks = nchunks;
	state->table.index = new_index(state, capacity, &size);
	state->table.index_len = size;
	check(state, state->kernels->table_create(state->table.index, size, state->nkeys,
											  state->kinds, payload_size,
											  capacity, &state->status));
	for (int chunk = AGG_EMPTY + 1; chunk < nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;

		check(state, state->kernels->table_link(&state->table, chunk, &from, NULL,
												NULL, &state->status));
	}
	dest = nchunks - 1;
	for (int chunk = 0; chunk < nsplit; chunk++)
	{
		agg_combine(state, spill, part, split[chunk], spill->chunk_len, &dest);
		pfree(split[chunk]);
	}
	if (split != NULL)
		pfree(split);
	reader = tess_spill_open(spill->file, 0, partition);
	while (reader != NULL && tess_spill_read_header(reader, &header))
	{
		void	   *body = MemoryContextAllocExtended(spill->block_context,
													  Max(header.len, 8),
													  MCXT_ALLOC_HUGE);

		tess_spill_read_body(reader, body, header.len);
		agg_combine(state, spill, part, body, header.len, &dest);
		MemoryContextReset(spill->block_context);
	}
	if (reader != NULL)
		tess_spill_close(reader);
	tess_spill_drop(spill->file, partition);
	state->table_bytes = size + part->bytes;
	note_memory(state);
}

/*
 * Before a level's partitions are given out: a partition with records on
 * disk writes its chunks in memory too, since it merges from disk anyway;
 * kept, they would narrow the room every other partition merges in, and
 * a partition that does not fit splits, writing all its records again.
 * Partitions wholly in memory stay.
 */
static void
agg_flush_spilled(TessAggState *state, AggSpill *spill)
{
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		AggPart    *part = &spill->parts[partition];

		if (part->disk_records == 0 || part->nchunks == 0)
			continue;
		for (int chunk = 0; chunk < part->nchunks; chunk++)
			agg_write_chunk(state, spill, partition, part->chunks[chunk]);
		part_release(spill, part);
	}
}

/*
 * A partition too large to merge splits by the next bits of the hash
 * into a level of its own: its chunks read back, then those in memory,
 * each split into the new level's partitions, which keep a chunk each in
 * memory and write the others. The new level is given out next.
 */
static void
agg_split_level(TessAggState *state, AggSpill *spill, int partition)
{
	AggPart    *part = &spill->parts[partition];
	AggSpill   *level = agg_spill_create(state, spill,
										 (double) part->disk_bytes + part->bytes,
										 spill->shift + pg_leftmost_one_pos32(spill->npartitions));
	TessSpillReader *reader = tess_spill_open(spill->file, 0, partition);
	TessSpillHeader header;

	state->splits++;
	while (reader != NULL && tess_spill_read_header(reader, &header))
	{
		void	   *body = MemoryContextAllocExtended(level->block_context,
													  Max(header.len, 8),
													  MCXT_ALLOC_HUGE);

		tess_spill_read_body(reader, body, header.len);
		agg_split(state, level, body, header.len, false);
		MemoryContextReset(level->block_context);
	}
	if (reader != NULL)
		tess_spill_close(reader);
	for (int chunk = 0; chunk < part->nchunks; chunk++)
		agg_split(state, level, part->chunks[chunk], spill->chunk_len, false);
	part_release(spill, part);
	tess_spill_drop(spill->file, partition);
	level->done_input = true;
	agg_flush_spilled(state, level);
	tess_spill_finish(level->file);
	state->spill = level;
	note_memory(state);
}

/*
 * The next partition to give out, merged into a table: after the input,
 * the partitions of the first level in turn, those wholly in memory
 * first, so that a partition read back from disk merges with the most
 * room; a level below given out whole where one split, and then the level
 * above again. False once every group is out.
 */
static bool
agg_advance(TessAggState *state)
{
	Size		limit = get_hash_memory_limit();

	for (;;)
	{
		AggSpill   *spill = state->spill;
		AggPart    *part;
		Size		others = 0;
		Size		size;

		if (spill->given)
			part_release(spill, &spill->parts[spill->partition]);
		spill->given = false;
		if (++spill->partition >= spill->npartitions)
		{
			if (spill->pass == 0)
			{
				spill->pass = 1;
				spill->partition = -1;
				continue;
			}
			if (spill->parent == NULL)
				return false;
			state->spill = spill->parent;
			agg_level_free(spill);
			continue;
		}
		part = &spill->parts[spill->partition];
		if (part->records == 0 && part->disk_records == 0)
			continue;
		if (spill->pass == 0 && part->disk_records > 0)
			continue;
		/*
		 * What the partition takes merged: a record per group and its
		 * index, and a block read back, next to the chunks every level
		 * keeps. A group written many times merges into one record, so the
		 * groups decide, not the file.
		 */
		size = part_groups(part) * (agg_record_size(state) + 2 * sizeof(uint64)) +
			spill->chunk_len;
		for (AggSpill *level = spill; level != NULL; level = level->parent)
			for (int partition = 0; partition < level->npartitions; partition++)
				if (level != spill || partition != spill->partition)
					others += level->parts[partition].bytes;
		if (part->disk_bytes > 0 && others + size > limit &&
			spill->shift + pg_leftmost_one_pos32(spill->npartitions) + 2 <= 32)
		{
			agg_split_level(state, spill, spill->partition);
			continue;
		}
		agg_merge(state, spill, spill->partition);
		spill->given = true;
		return true;
	}
}

/* The input is done: the partitions are given out one by one. */
static void
agg_finish_input(TessAggState *state)
{
	AggSpill   *spill = state->spill;

	spill->done_input = true;
	spill->partition = -1;
	agg_flush_spilled(state, spill);
	tess_spill_finish(spill->file);
	if (state->table.index != NULL)
		pfree(state->table.index);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	state->cursor = 0;
	if (!agg_advance(state))
		state->table.nchunks = 0;
}

static Size
agg_spill_memory(TessAggState *state)
{
	Size		memory = state->table.index != NULL ? state->table.index_len : 0;

	for (AggSpill *spill = state->spill; spill != NULL; spill = spill->parent)
		memory += MemoryContextMemAllocated(spill->context, true);
	return memory;
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
	if (state->spill != NULL)
		agg_find_partitioned(state, &pending, &inserted);
	else if (state->table.nchunks == 0)
		add_chunk(state);
	for (; state->spill == NULL;)
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

		TessRowMask rows = valid;

		if (value->computed >= 0)
			computed_column(state, batch, value->computed,
							TESS_COLUMN_FOR_PROJECTION, &column);
		if (value->distinct != NULL)
			rows = distinct_rows(state, value, nrows, state->hashes, &valid,
								 &column);
		state->calls++;
		check(state, state->kernels->table_accumulate(&state->table,
													  state->offsets, &rows,
													  value->accumulate,
													  value->computed >= 0 ? &column : NULL,
													  NULL,
													  sizeof(uint64) * (1 + index),
													  0, (uint32) index,
													  &state->status));
	}
	/*
	 * Past seven eighths of hash_mem, the rest left for a batch's chunk and
	 * index: the groups go into partitions, and the largest to disk; in
	 * partial mode they go out instead (group_drain).
	 */
	if (state->spill == NULL && (!state->partial || state->partial_spill) &&
		!state->has_distinct &&
		state->table_bytes > get_hash_memory_limit() / 8 * 7)
		agg_start_spill(state);
	if (state->spill != NULL)
		agg_make_room(state);
}

/* Read every batch of the child into the table of groups. */
static void
group_drain(TessAggState *state)
{
	agg_spill_free(state);
	create_table(state);
	reset_distinct(state);
	for (;;)
	{
		TessBatch  *batch;
		int			rows;

		/*
		 * Partial mode: a table near hash_mem goes out now, as partials the
		 * Finalize Aggregate merges, and the input goes on after it.
		 */
		if (state->partial && !state->partial_spill &&
			state->table_bytes > get_hash_memory_limit() / 8 * 7)
		{
			TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

			check(state, state->kernels->table_stats(&state->table, &stats,
													 &state->status));
			/*
			 * More groups than half the rows read since the table started:
			 * sending it up would fold nothing. The groups go into
			 * partitions and to disk from now on, and out once the input is
			 * done, still as partials.
			 */
			if (stats.records * 2 > state->rows - state->emit_rows)
			{
				state->partial_spill = true;
				agg_start_spill(state);
				continue;
			}
			state->emit_rows = state->rows;
			state->early_emits++;
			state->drained = true;
			state->cursor = 0;
			return;
		}
		batch = tess_input_next(state->input);
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
	state->input_done = true;
	state->cursor = 0;
	if (state->spill != NULL)
		agg_finish_input(state);
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

		/* A table that spilled has no index once every partition is out. */
		if (state->table.index == NULL)
			return NULL;
		check(state, state->kernels->table_scan(&state->table,
												&state->cursor, state->walked,
												AGG_GROUP_ROWS, &count,
												&state->status));
		if (count == 0)
		{
			/* Partial mode: the rest of the input into a table anew. */
			if (state->spill == NULL && !state->input_done)
			{
				group_drain(state);
				continue;
			}
			/* The next partition of a table that spilled. */
			if (state->spill == NULL || !agg_advance(state))
				return NULL;
			state->cursor = 0;
			continue;
		}
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
	agg_spill_free(state);
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
	agg_spill_free(state);
	state->drained = false;
	state->input_done = false;
	state->published = NULL;
	state->groups = 0;
	state->emit_rows = 0;
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
	if (state->projection != NULL)
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
		if (totals[AGG_EARLY] > 0)
			ExplainPropertyInteger("Early Emits", NULL, totals[AGG_EARLY], es);
		if (totals[AGG_PARTITIONS] > 0)
		{
			ExplainPropertyInteger("Batches", NULL, totals[AGG_PARTITIONS], es);
			ExplainPropertyInteger("Evictions", NULL, totals[AGG_EVICTIONS], es);
			ExplainPropertyInteger("Spilled Chunks", NULL, totals[AGG_SPILLED], es);
			ExplainPropertyInteger("Disk Usage", "kB", (totals[AGG_DISK] + 1023) / 1024, es);
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

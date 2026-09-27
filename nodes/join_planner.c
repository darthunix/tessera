#include "postgres.h"

#include "catalog/pg_type_d.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "optimizer/restrictinfo.h"
#include "utils/fmgroids.h"

#include "tessera/expr.h"
#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * The planner of TessHashJoin: through set_join_pathlist_hook it offers
 * the node for an inner join with an equality of an int4 or int8 column
 * of each side, the others such keys too or residual clauses over the
 * joined rows, with a cost below the core's hash join of
 * the same inputs, which serves as the template. The children are batch
 * paths over the sides' cheapest paths. See docs/nodes.md.
 */
#define JOIN_COST_FACTOR 0.9
/* The executor keeps each inner column in a payload word; see hashjoin.c. */
#define JOIN_MAX_INNER_COLUMNS 64

static set_join_pathlist_hook_type previous_set_join_pathlist_hook = NULL;

static Plan *join_plan(PlannerInfo *root, RelOptInfo *rel,
					   CustomPath *best_path, List *tlist, List *clauses,
					   List *custom_plans);

static const CustomPathMethods join_path_methods = {
	.CustomName = "TessHashJoin",
	.PlanCustomPath = join_plan,
};

/* The join clauses: each one's column on each side and its kind there. */
typedef struct JoinKeys
{
	int			nkeys;
	List	   *rinfos;
	List	   *clauses;
	List	   *outer;
	List	   *inner;
	List	   *outer_kinds;
	List	   *inner_kinds;
	/* The other join clauses, which decide with the keys whether rows match. */
	List	   *residual;
	/*
	 * LEFT and ANTI: the clauses pushed down to the join from above, a
	 * filter over the rows it returns, after NULL extension.
	 */
	List	   *filters;
} JoinKeys;

/* The table's kind of a key of this type, or false for another type. */
static bool
key_kind(Oid type, TessTableKeyKind *kind)
{
	if (type == INT4OID)
		*kind = TESS_TABLE_KEY_INT4;
	else if (type == INT8OID)
		*kind = TESS_TABLE_KEY_INT8;
	else
		return false;
	return true;
}

/* A column of the query level: the only operand a key may be. */
static bool
plain_var(Node *node)
{
	return IsA(node, Var) && ((Var *) node)->varlevelsup == 0;
}

/*
 * One clause as a key, when it is an integer equality of a column of each
 * side: int4 and int8 in any combination, since the table keeps both as
 * 8-byte keys and an int8 in the int4 range hashes as the int4.
 */
static bool
add_key(RestrictInfo *rinfo, RelOptInfo *outerrel, RelOptInfo *innerrel,
		JoinKeys *keys)
{
	OpExpr	   *op;
	Node	   *left;
	Node	   *right;
	Var		   *outer;
	Var		   *inner;
	TessTableKeyKind outer_kind;
	TessTableKeyKind inner_kind;

	if (rinfo->pseudoconstant || !rinfo->can_join ||
		!OidIsValid(rinfo->hashjoinoperator) || !IsA(rinfo->clause, OpExpr))
		return false;
	op = (OpExpr *) rinfo->clause;
	set_opfuncid(op);
	if (op->opfuncid != F_INT4EQ && op->opfuncid != F_INT8EQ &&
		op->opfuncid != F_INT48EQ && op->opfuncid != F_INT84EQ)
		return false;
	left = linitial(op->args);
	right = lsecond(op->args);
	if (!plain_var(left) || !plain_var(right))
		return false;
	if (bms_is_subset(rinfo->left_relids, outerrel->relids) &&
		bms_is_subset(rinfo->right_relids, innerrel->relids))
	{
		outer = (Var *) left;
		inner = (Var *) right;
	}
	else if (bms_is_subset(rinfo->left_relids, innerrel->relids) &&
			 bms_is_subset(rinfo->right_relids, outerrel->relids))
	{
		outer = (Var *) right;
		inner = (Var *) left;
	}
	else
		return false;
	if (!key_kind(outer->vartype, &outer_kind) ||
		!key_kind(inner->vartype, &inner_kind))
		return false;
	keys->rinfos = lappend(keys->rinfos, rinfo);
	keys->clauses = lappend(keys->clauses, rinfo->clause);
	keys->outer = lappend(keys->outer, outer);
	keys->inner = lappend(keys->inner, inner);
	keys->outer_kinds = lappend_int(keys->outer_kinds, outer_kind);
	keys->inner_kinds = lappend_int(keys->inner_kinds, inner_kind);
	keys->nkeys++;
	return true;
}

/*
 * A residual clause the node evaluates over the joined rows: one reading
 * plain columns of the query level, no placeholder, and no pseudoconstant,
 * which the planner gates the whole join with instead.
 */
static bool
residual_supported(RestrictInfo *rinfo)
{
	if (rinfo->pseudoconstant)
		return false;
	foreach_ptr(Node, node, pull_var_clause((Node *) rinfo->clause,
											PVC_INCLUDE_PLACEHOLDERS))
	{
		if (!plain_var(node))
			return false;
	}
	return true;
}

/*
 * The join's clauses: its integer equalities between the sides as keys,
 * as many as the table takes, and the others as residual clauses, in the
 * order the core's hash join evaluates them, which sorts the join's
 * clauses by cost within security levels. An outer join (LEFT, ANTI)
 * splits them as the core does: a clause pushed down from above is no
 * join clause but a filter over the joined rows, and never a key. There
 * must be a key.
 */
static bool
find_keys(PlannerInfo *root, RelOptInfo *joinrel, List *restrictlist,
		  RelOptInfo *outerrel, RelOptInfo *innerrel, JoinType jointype,
		  JoinKeys *keys)
{
	List	   *residual = NIL;
	List	   *filters = NIL;

	memset(keys, 0, sizeof(*keys));
	foreach_node(RestrictInfo, rinfo, restrictlist)
	{
		if (IS_OUTER_JOIN(jointype) && RINFO_IS_PUSHED_DOWN(rinfo, joinrel->relids))
		{
			if (!residual_supported(rinfo))
				return false;
			filters = lappend(filters, rinfo);
			continue;
		}
		if (keys->nkeys < TESS_TABLE_MAX_KEYS &&
			add_key(rinfo, outerrel, innerrel, keys))
			continue;
		if (!residual_supported(rinfo))
			return false;
		residual = lappend(residual, rinfo);
	}
	foreach_node(RestrictInfo, rinfo, tess_order_clauses(root, residual))
		keys->residual = lappend(keys->residual, rinfo->clause);
	foreach_node(RestrictInfo, rinfo, tess_order_clauses(root, filters))
		keys->filters = lappend(keys->filters, rinfo->clause);
	return keys->nkeys > 0;
}

/*
 * The join's target is plain columns, which the node passes through or
 * keeps in the payload, and the inner ones fit there. The count is the
 * inner columns of the target and the inner key.
 */
static bool
target_supported(RelOptInfo *joinrel, RelOptInfo *innerrel, const JoinKeys *keys,
				 int *ninner)
{
	Bitmapset  *inner_keys = NULL;

	*ninner = 0;
	foreach_ptr(Node, expr, joinrel->reltarget->exprs)
	{
		Var		   *var;

		if (!plain_var(expr))
			return false;
		var = (Var *) expr;
		if (!bms_is_member(var->varno, innerrel->relids))
			continue;
		(*ninner)++;
		foreach_node(Var, key, keys->inner)
		{
			if (var->varno == key->varno && var->varattno == key->varattno)
				inner_keys = bms_add_member(inner_keys,
											foreach_current_index(key));
		}
	}
	/* Keys outside the target are columns of the scan tuple too. */
	*ninner += keys->nkeys - bms_num_members(inner_keys);
	/* So are the clauses' inner columns, counted generously. */
	foreach_node(Var, var, pull_var_clause((Node *) list_make2(keys->residual,
															   keys->filters), 0))
	{
		if (bms_is_member(var->varno, innerrel->relids))
			(*ninner)++;
	}
	return *ninner <= JOIN_MAX_INNER_COLUMNS;
}

/*
 * The path: the core's hash join of the same inputs as the template, at a
 * lower cost, over batch paths of them; the template's cost counts the
 * batches the core would write, and the node spills as the core does, a
 * shared table past every participant's hash_mem too.
 */
static CustomPath *
make_join_path(PlannerInfo *root, RelOptInfo *joinrel, JoinType jointype,
			   JoinPathExtraData *extra, const JoinKeys *keys,
			   Path *outer_path, Path *inner_path, bool shared)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	JoinCostWorkspace workspace;
	List	   *hashclauses = keys->rinfos;
	HashPath   *template;
	Path	   *outer;
	Path	   *inner;

	/* A partial inner path's rows are one participant's share. */
	double		inner_rows = shared ?
		inner_path->rows * tess_parallel_divisor(inner_path) : inner_path->rows;

	initial_cost_hashjoin(root, &workspace, jointype, hashclauses,
						  outer_path, inner_path, extra, shared);
	outer = tess_batch_input_path(root, outer_path);
	inner = tess_batch_input_path(root, inner_path);
	if (outer == NULL || inner == NULL)
		return NULL;
	/* The core's clause sides, which its costing reads. */
	foreach_node(RestrictInfo, rinfo, keys->rinfos)
		rinfo->outer_is_left = bms_is_subset(rinfo->left_relids,
											 outer_path->parent->relids);
	template = create_hashjoin_path(root, joinrel, jointype, &workspace,
									extra, outer_path, inner_path, shared,
									extra->restrictlist, NULL, hashclauses);
	template->jpath.path.total_cost *= JOIN_COST_FACTOR;
	config.template_path = &template->jpath.path;
	config.methods = &join_path_methods;
	config.node = &tess_hash_join_node;
	config.children = list_make2(outer, inner);
	/* A target above the join becomes the node's, computed over the pairs. */
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	config.expressions = list_make5(keys->clauses, keys->outer, keys->inner,
									keys->residual, keys->filters);
	config.node_data = (Node *) list_make3(keys->outer_kinds, keys->inner_kinds,
										   list_make4_int(extra->inner_unique ? 1 : 0,
														  (int) Min(inner_rows,
																	(double) PG_INT32_MAX),
														  (int) jointype,
														  shared ? 1 : 0));
	return tess_path_create(&config);
}

static void
add_join_paths(PlannerInfo *root, RelOptInfo *joinrel, RelOptInfo *outerrel,
			   RelOptInfo *innerrel, JoinType jointype,
			   JoinPathExtraData *extra)
{
	JoinKeys	keys;
	int			ninner;
	Path	   *outer_path = outerrel->cheapest_total_path;
	Path	   *inner_path = innerrel->cheapest_total_path;
	CustomPath *path;

	/*
	 * The core offers no hash join either without PGS_HASHJOIN. The kinds
	 * that keep the outer side, which the node probes with: each outer
	 * row's matches are known when its batch has been probed; RIGHT and
	 * FULL keep the inner side too, marking the table's records that find
	 * a pair and returning the others after the outer side.
	 */
	if (!*tess_runtime_api()->settings->enable ||
		(jointype != JOIN_INNER && jointype != JOIN_SEMI &&
		 jointype != JOIN_ANTI && jointype != JOIN_LEFT &&
		 jointype != JOIN_RIGHT && jointype != JOIN_FULL) ||
		(extra->pgs_mask & PGS_HASHJOIN) == 0 ||
		!find_keys(root, joinrel, extra->restrictlist, outerrel, innerrel,
				   jointype, &keys) ||
		!target_supported(joinrel, innerrel, &keys, &ninner) ||
		outer_path == NULL || inner_path == NULL ||
		PATH_REQ_OUTER(outer_path) != NULL || PATH_REQ_OUTER(inner_path) != NULL ||
		tess_runtime_kernels() == NULL)
		return;
	/*
	 * An inner or semi join drops an outer row without a pair: over a
	 * relation whose clauses all run row by row, TessFilter takes them
	 * from the core scan, and the join's Bloom filter reaches them.
	 */
	if (jointype == JOIN_INNER || jointype == JOIN_SEMI)
	{
		Path	   *filtered = tess_filter_row_path(root, outerrel, outer_path);

		if (filtered != NULL)
			outer_path = filtered;
	}
	path = make_join_path(root, joinrel, jointype, extra, &keys,
						  outer_path, inner_path, false);
	if (path != NULL)
		add_path(joinrel, &path->path);

	/*
	 * Under a Gather: the outer side's cheapest partial path divides the
	 * rows, and every participant builds the whole inner side, as the
	 * core's hash join without a shared table does, from the cheapest
	 * inner path a worker may run. The path is parallel-aware for the
	 * counters the node shares. RIGHT and FULL take a shared table only,
	 * as the core does: with a table each, every participant would return
	 * the records without a pair.
	 */
	if (!joinrel->consider_parallel || outerrel->partial_pathlist == NIL ||
		!bms_is_empty(joinrel->lateral_relids))
		return;
	if (!inner_path->parallel_safe)
		inner_path = get_cheapest_parallel_safe_total_inner(innerrel->pathlist);
	if (inner_path == NULL)
		return;
	outer_path = linitial(outerrel->partial_pathlist);
	if (jointype == JOIN_INNER || jointype == JOIN_SEMI)
	{
		Path	   *filtered = tess_filter_row_path(root, outerrel, outer_path);

		if (filtered != NULL)
			outer_path = filtered;
	}
	path = jointype == JOIN_RIGHT || jointype == JOIN_FULL ? NULL :
		make_join_path(root, joinrel, jointype, extra, &keys,
					   outer_path, inner_path, false);
	if (path != NULL && path->path.parallel_safe && path->path.parallel_workers > 0)
	{
		path->path.parallel_aware = true;
		add_partial_path(joinrel, &path->path);
	}

	/*
	 * A shared table, where the core offers a Parallel Hash: the inner
	 * side's partial path divides the build among the participants too,
	 * into one table in the query's shared memory.
	 */
	if (!enable_parallel_hash || innerrel->partial_pathlist == NIL)
		return;
	path = make_join_path(root, joinrel, jointype, extra, &keys,
						  outer_path, linitial(innerrel->partial_pathlist), true);
	if (path == NULL || !path->path.parallel_safe ||
		path->path.parallel_workers <= 0)
		return;
	path->path.parallel_aware = true;
	add_partial_path(joinrel, &path->path);
}

/* The node's paths, then TessGather over the cheapest partial path, before the core gathers it. */
static void
join_pathlist(PlannerInfo *root, RelOptInfo *joinrel, RelOptInfo *outerrel,
			  RelOptInfo *innerrel, JoinType jointype,
			  JoinPathExtraData *extra)
{
	if (previous_set_join_pathlist_hook != NULL)
		previous_set_join_pathlist_hook(root, joinrel, outerrel, innerrel,
										jointype, extra);
	add_join_paths(root, joinrel, outerrel, innerrel, jointype, extra);
	tess_gather_add_paths(root, joinrel);
}


/* The target entry of a child's plan that is this column, or NULL. */
static TargetEntry *
child_entry(const Plan *child, const Var *var)
{
	foreach_ptr(TargetEntry, entry, child->targetlist)
	{
		Var		   *other = (Var *) entry->expr;

		if (IsA(other, Var) && other->varno == var->varno &&
			other->varattno == var->varattno &&
			other->varlevelsup == var->varlevelsup)
			return entry;
	}
	return NULL;
}

/* The batch column of a child that holds this column. */
static int
child_column(const TessPlanChild *child, const Var *var)
{
	TargetEntry *entry = child_entry(child->plan, var);
	int			column = entry == NULL ? -1 :
		tess_layout_column(&child->layout, entry->resno - 1);

	if (column < 0)
		elog(ERROR, "TessHashJoin found a column missing from its child");
	return column;
}

/* The scan tuple's entry of this column, or NULL. */
static Var *
scan_entry(List *scan, const Var *var)
{
	foreach_ptr(TargetEntry, entry, scan)
	{
		Var		   *other = (Var *) entry->expr;

		if (other->varno == var->varno && other->varattno == var->varattno)
			return other;
	}
	return NULL;
}

/* Whether the scan tuple already has this column. */
static bool
scan_has(List *scan, const Var *var)
{
	return scan_entry(scan, var) != NULL;
}

/*
 * Above a LEFT join an inner column is marked as nulled by it, while the
 * join's own clauses read it unmarked; the planner matches every Var of
 * custom_exprs against the scan tuple's one entry of its column, marks
 * included. The clauses' Vars take the entry's marks, which mean nothing
 * to the executor: it reads the scan tuple's column either way.
 */
static Node *
align_nullingrels(Node *node, List *scan)
{
	if (node == NULL)
		return NULL;
	if (IsA(node, Var))
	{
		Var		   *entry = scan_entry(scan, (Var *) node);
		Var		   *var;

		if (entry == NULL ||
			bms_equal(entry->varnullingrels, ((Var *) node)->varnullingrels))
			return node;
		var = copyObject((Var *) node);
		var->varnullingrels = bms_copy(entry->varnullingrels);
		return (Node *) var;
	}
	return expression_tree_mutator(node, align_nullingrels, scan);
}

/*
 * The scan tuple is the join's columns, the outer side's first, then the
 * keys and the residual clauses' columns, which custom_exprs refers to; the node's targets are columns of it. Each entry is a column
 * of one child's batches, which the plan data records.
 */
static Plan *
join_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		  List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPlanChild outer = TESS_STRUCT_INITIALIZER(TessPlanChild);
	TessPlanChild inner = TESS_STRUCT_INITIALIZER(TessPlanChild);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	TessPlanWriter *writer;
	List	   *data;
	List	   *hash_clauses;
	List	   *outer_keys;
	List	   *inner_keys;
	List	   *residual;
	List	   *filters;
	List	   *residual_batch = NIL;
	List	   *filter_batch = NIL;
	List	   *outer_columns = NIL;
	List	   *inner_columns = NIL;
	Relids		outer_relids;
	List	   *scan = NIL;
	List	   *sides = NIL;
	List	   *columns = NIL;

	tess_path_get_info(best_path, &info);
	if (!tess_plan_child(best_path, custom_plans, 0, &outer) ||
		!tess_plan_child(best_path, custom_plans, 1, &inner) ||
		list_length(info.expressions) != 5)
		elog(ERROR, "TessHashJoin expected two batch children");
	hash_clauses = linitial(info.expressions);
	outer_keys = lsecond(info.expressions);
	inner_keys = lthird(info.expressions);
	residual = lfourth(info.expressions);
	filters = list_nth(info.expressions, 4);
	/* In evaluation order, each in batches if the compiler takes it, else by rows. */
	foreach_ptr(Node, clause, residual)
		residual_batch = lappend_int(residual_batch,
									 tess_expr_supports_filter(clause, 0) ? 1 : 0);
	foreach_ptr(Node, clause, filters)
		filter_batch = lappend_int(filter_batch,
								   tess_expr_supports_filter(clause, 0) ? 1 : 0);
	data = (List *) info.node_data;
	outer_relids = outer.path->parent->relids;
	for (int side = 0; side < 2; side++)
	{
		List	   *wanted = NIL;

		/*
		 * The join's columns: the targets above read them, and PostgreSQL
		 * plans the node without a target list when it puts a projection
		 * into the node's plan afterwards. The targets' own columns too,
		 * when an expression target came down with the path.
		 */
		foreach_node(Var, var, pull_var_clause((Node *) rel->reltarget->exprs, 0))
			wanted = lappend(wanted, makeTargetEntry((Expr *) var, 0, NULL, true));
		foreach_node(Var, var, pull_var_clause((Node *) tlist, 0))
			wanted = lappend(wanted, makeTargetEntry((Expr *) var, 0, NULL, true));
		foreach_node(Var, key, side == 0 ? outer_keys : inner_keys)
			wanted = lappend(wanted, makeTargetEntry((Expr *) key, 0, NULL, true));
		/* The clauses' columns, which the quals refer to. */
		foreach_node(Var, var, pull_var_clause((Node *) list_make2(residual, filters), 0))
			wanted = lappend(wanted, makeTargetEntry((Expr *) var, 0, NULL, true));
		foreach_ptr(TargetEntry, entry, wanted)
		{
			Var		   *var = (Var *) entry->expr;

			if (!IsA(var, Var))
				elog(ERROR, "TessHashJoin expected columns in its scan tuple");
			if (bms_is_member(var->varno, outer_relids) != (side == 0) ||
				scan_has(scan, var))
				continue;
			scan = lappend(scan, makeTargetEntry((Expr *) copyObject(var),
												 list_length(scan) + 1, NULL,
												 false));
			sides = lappend_int(sides, side);
			columns = lappend_int(columns,
								  child_column(side == 0 ? &outer : &inner, var));
		}
	}
	layout.ncolumns = list_length(scan);
	layout.ntargets = list_length(scan);

	writer = tess_plan_writer_create(TESS_HASH_JOIN_DATA,
									 TESS_HASH_JOIN_DATA_VERSION);
	tess_plan_write_int_list(writer, "sides", sides);
	tess_plan_write_int_list(writer, "child_columns", columns);
	foreach_node(Var, key, outer_keys)
		outer_columns = lappend_int(outer_columns, child_column(&outer, key));
	foreach_node(Var, key, inner_keys)
		inner_columns = lappend_int(inner_columns, child_column(&inner, key));
	tess_plan_write_int_list(writer, "outer_keys", outer_columns);
	tess_plan_write_int_list(writer, "inner_keys", inner_columns);
	tess_plan_write_int_list(writer, "outer_kinds", linitial(data));
	tess_plan_write_int_list(writer, "inner_kinds", lsecond(data));
	tess_plan_write_int_list(writer, "residual_batch", residual_batch);
	tess_plan_write_int_list(writer, "filter_batch", filter_batch);
	tess_plan_write_int(writer, "jointype", lthird_int(lthird(data)));
	tess_plan_write_int(writer, "inner_unique", linitial_int(lthird(data)));
	tess_plan_write_int(writer, "inner_rows", lsecond_int(lthird(data)));
	tess_plan_write_int(writer, "shared", list_length(lthird(data)) > 3 ?
						lfourth_int(lthird(data)) : 0);

	config.methods = &tess_hash_join_scan_methods;
	config.layout_policy = TESS_LAYOUT_PROJECTED;
	config.explicit_layout = &layout;
	/*
	 * The key clauses, the residual join clauses and the filters of an
	 * outer join, each in evaluation order: the node shows and applies
	 * them all, and a plan qual would also be shown by EXPLAIN as a filter.
	 */
	config.qual = NIL;
	config.expressions = (List *)
		align_nullingrels((Node *) list_concat(list_concat_copy(hash_clauses, residual),
											   filters), scan);
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	config.scan_targetlist = scan;
	config.scanrelid = 0;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

void
tess_hash_join_planner_init(void)
{
	previous_set_join_pathlist_hook = set_join_pathlist_hook;
	set_join_pathlist_hook = join_pathlist;
}

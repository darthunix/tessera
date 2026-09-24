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

#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * The planner of TessHashJoin: through set_join_pathlist_hook it offers
 * the node for an inner join whose only clause is an equality of an int4
 * or int8 column of each side, with a cost below the core's hash join of
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

/* The join clause and its key on each side. */
typedef struct JoinKey
{
	RestrictInfo *rinfo;
	Var		   *outer;
	Var		   *inner;
	TessTableKeyKind outer_kind;
	TessTableKeyKind inner_kind;
} JoinKey;

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
 * The join's only clause, when it is an integer equality of a column of
 * each side: int4 and int8 in any combination, since the table keeps both
 * as 8-byte keys and an int8 in the int4 range hashes as the int4.
 */
static bool
find_key(List *restrictlist, RelOptInfo *outerrel, RelOptInfo *innerrel,
		 JoinKey *key)
{
	RestrictInfo *rinfo;
	OpExpr	   *op;
	Node	   *left;
	Node	   *right;

	if (list_length(restrictlist) != 1)
		return false;
	rinfo = linitial_node(RestrictInfo, restrictlist);
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
		key->outer = (Var *) left;
		key->inner = (Var *) right;
	}
	else if (bms_is_subset(rinfo->left_relids, innerrel->relids) &&
			 bms_is_subset(rinfo->right_relids, outerrel->relids))
	{
		key->outer = (Var *) right;
		key->inner = (Var *) left;
	}
	else
		return false;
	key->rinfo = rinfo;
	return key_kind(key->outer->vartype, &key->outer_kind) &&
		key_kind(key->inner->vartype, &key->inner_kind);
}

/*
 * The join's target is plain columns, which the node passes through or
 * keeps in the payload, and the inner ones fit there. The count is the
 * inner columns of the target and the inner key.
 */
static bool
target_supported(RelOptInfo *joinrel, RelOptInfo *innerrel, const JoinKey *key,
				 int *ninner)
{
	bool		inner_key = false;

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
		inner_key |= var->varno == key->inner->varno &&
			var->varattno == key->inner->varattno;
	}
	if (!inner_key)
		(*ninner)++;
	return *ninner <= JOIN_MAX_INNER_COLUMNS;
}

/*
 * The table's bytes for the inner rows: a record of a header, the key and
 * the payload (the NULL bits and a word per column), by-reference values
 * copied by their width, and the buckets, a power of two at least twice
 * the records.
 */
static double
table_bytes(Path *inner, int ninner)
{
	double		rows = Max(inner->rows, 1.0);
	double		buckets = 1024;

	while (buckets < 2 * rows)
		buckets *= 2;
	return rows * (16 + 8 + 8 * (1 + ninner) + inner->pathtarget->width) +
		buckets * 4;
}

/*
 * The path: the core's hash join of the same inputs as the template, at a
 * lower cost, over batch paths of them. NULL when the core would split the
 * inner side into batches or the table would not fit in hash_mem: the
 * node keeps the whole table in memory.
 */
static CustomPath *
make_join_path(PlannerInfo *root, RelOptInfo *joinrel,
			   JoinPathExtraData *extra, const JoinKey *key, int ninner,
			   Path *outer_path, Path *inner_path)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	JoinCostWorkspace workspace;
	List	   *hashclauses = list_make1(key->rinfo);
	HashPath   *template;
	Path	   *outer;
	Path	   *inner;

	initial_cost_hashjoin(root, &workspace, JOIN_INNER, hashclauses,
						  outer_path, inner_path, extra, false);
	if (workspace.numbatches > 1 ||
		table_bytes(inner_path, ninner) > (double) get_hash_memory_limit())
		return NULL;
	outer = tess_batch_input_path(root, outer_path);
	inner = tess_batch_input_path(root, inner_path);
	if (outer == NULL || inner == NULL)
		return NULL;
	/* The core's clause sides, which its costing reads. */
	key->rinfo->outer_is_left = bms_is_subset(key->rinfo->left_relids,
											  outer_path->parent->relids);
	template = create_hashjoin_path(root, joinrel, JOIN_INNER, &workspace,
									extra, outer_path, inner_path, false,
									extra->restrictlist, NULL, hashclauses);
	template->jpath.path.total_cost *= JOIN_COST_FACTOR;
	config.template_path = &template->jpath.path;
	config.methods = &join_path_methods;
	config.node = &tess_hash_join_node;
	config.children = list_make2(outer, inner);
	config.expressions = list_make3(key->rinfo->clause, key->outer, key->inner);
	config.node_data = (Node *) list_make4_int(key->outer_kind, key->inner_kind,
											   extra->inner_unique ? 1 : 0,
											   (int) Min(inner_path->rows,
														 (double) PG_INT32_MAX));
	return tess_path_create(&config);
}

static void
join_pathlist(PlannerInfo *root, RelOptInfo *joinrel, RelOptInfo *outerrel,
			  RelOptInfo *innerrel, JoinType jointype,
			  JoinPathExtraData *extra)
{
	JoinKey		key;
	int			ninner;
	Path	   *outer_path = outerrel->cheapest_total_path;
	Path	   *inner_path = innerrel->cheapest_total_path;
	CustomPath *path;

	if (previous_set_join_pathlist_hook != NULL)
		previous_set_join_pathlist_hook(root, joinrel, outerrel, innerrel,
										jointype, extra);
	/* The core offers no hash join either without PGS_HASHJOIN. */
	if (!*tess_runtime_api()->settings->enable || jointype != JOIN_INNER ||
		(extra->pgs_mask & PGS_HASHJOIN) == 0 ||
		!find_key(extra->restrictlist, outerrel, innerrel, &key) ||
		!target_supported(joinrel, innerrel, &key, &ninner) ||
		outer_path == NULL || inner_path == NULL ||
		PATH_REQ_OUTER(outer_path) != NULL || PATH_REQ_OUTER(inner_path) != NULL ||
		tess_runtime_kernels() == NULL)
		return;
	path = make_join_path(root, joinrel, extra, &key, ninner, outer_path,
						  inner_path);
	if (path != NULL)
		add_path(joinrel, &path->path);
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

/* Whether the scan tuple already has this column. */
static bool
scan_has(List *scan, const Var *var)
{
	foreach_ptr(TargetEntry, entry, scan)
	{
		Var		   *other = (Var *) entry->expr;

		if (other->varno == var->varno && other->varattno == var->varattno)
			return true;
	}
	return false;
}

/*
 * The scan tuple is the join's columns, the outer side's first, and the
 * keys, which the join clause in custom_exprs refers to; the node's
 * targets are columns of it. Each entry is a column of one child's
 * batches, which the plan data records.
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
	Node	   *clause;
	Var		   *outer_key;
	Var		   *inner_key;
	Relids		outer_relids;
	List	   *scan = NIL;
	List	   *sides = NIL;
	List	   *columns = NIL;

	tess_path_get_info(best_path, &info);
	if (!tess_plan_child(best_path, custom_plans, 0, &outer) ||
		!tess_plan_child(best_path, custom_plans, 1, &inner) ||
		list_length(info.expressions) != 3)
		elog(ERROR, "TessHashJoin expected two batch children");
	clause = linitial(info.expressions);
	outer_key = lsecond_node(Var, info.expressions);
	inner_key = lthird_node(Var, info.expressions);
	data = (List *) info.node_data;
	outer_relids = outer.path->parent->relids;
	for (int side = 0; side < 2; side++)
	{
		List	   *wanted = list_copy(tlist);

		wanted = lappend(wanted, makeTargetEntry((Expr *) (side == 0 ? outer_key : inner_key),
												 0, NULL, true));
		foreach_ptr(TargetEntry, entry, wanted)
		{
			Var		   *var = (Var *) entry->expr;

			if (!IsA(var, Var))
				elog(ERROR, "TessHashJoin expected plain columns as its targets");
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
	tess_plan_write_int(writer, "outer_key", child_column(&outer, outer_key));
	tess_plan_write_int(writer, "inner_key", child_column(&inner, inner_key));
	tess_plan_write_int(writer, "outer_kind", linitial_int(data));
	tess_plan_write_int(writer, "inner_kind", lsecond_int(data));
	tess_plan_write_int(writer, "inner_unique", lthird_int(data));
	tess_plan_write_int(writer, "inner_rows", lfourth_int(data));

	config.methods = &tess_hash_join_scan_methods;
	config.layout_policy = TESS_LAYOUT_PROJECTED;
	config.explicit_layout = &layout;
	config.qual = NIL;
	config.expressions = list_make1(clause);
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

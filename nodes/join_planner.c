#include "postgres.h"

#include <math.h>

#include "access/stratnum.h"
#include "catalog/pg_statistic.h"
#include "catalog/pg_type_d.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/appendinfo.h"
#include "optimizer/clauses.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/paramassign.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "optimizer/restrictinfo.h"
#include "optimizer/tlist.h"
#include "optimizer/tlist.h"
#include "partitioning/partprune.h"
#include "utils/datum.h"
#include "utils/fmgroids.h"
#include "utils/lsyscache.h"
#include "utils/selfuncs.h"
#include "utils/typcache.h"

#include "tessera/expr.h"
#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * The planner of TessHashJoin: through set_join_pathlist_hook it offers
 * the node for a join with an equality of a column of each side, of words
 * or of a type whose equality hashes, the others such keys too or
 * residual clauses over the joined rows, with a cost below the core's
 * hash join of the same inputs, which serves as the template. The
 * children are batch paths over the sides' cheapest paths. See
 * docs/nodes.md.
 */
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
	/*
	 * A key a word does not hold (text, numeric, ...): the table keeps the
	 * 64-bit hash of its value by its type's function, 0 for a word key, and
	 * the clause is also a residual one, which decides whether two rows of
	 * one hash match.
	 */
	List	   *hashers;
	List	   *collations;
	/* The other join clauses, which decide with the keys whether rows match. */
	List	   *residual;
	/*
	 * LEFT and ANTI: the clauses pushed down to the join from above, a
	 * filter over the rows it returns, after NULL extension.
	 */
	List	   *filters;
} JoinKeys;

/*
 * Whether an equality compares its operands' words bit for bit, as the
 * table does: of integers in any combination, since the table keeps int2,
 * int4 and int8 as 8-byte keys and an int8 in the range of a smaller type
 * hashes as that type's; of two dates, of two timestamps, of two
 * timestamps with time zone, of two booleans.
 */
static bool
word_equality(Oid funcid)
{
	switch (funcid)
	{
		case F_INT2EQ:
		case F_INT4EQ:
		case F_INT8EQ:
		case F_INT24EQ:
		case F_INT42EQ:
		case F_INT28EQ:
		case F_INT82EQ:
		case F_INT48EQ:
		case F_INT84EQ:
		case F_DATE_EQ:
		case F_TIMESTAMP_EQ:
		case F_TIMESTAMPTZ_EQ:
		case F_BOOLEQ:
			return true;
		default:
			return false;
	}
}

/* A column of the query level: the only operand a key may be. */
static bool
plain_var(Node *node)
{
	return IsA(node, Var) && ((Var *) node)->varlevelsup == 0;
}

/*
 * An operand under a binary coercion, which changes no value: a varchar
 * column compared as text is the column.
 */
static Node *
strip_relabel(Node *node)
{
	while (IsA(node, RelabelType))
		node = (Node *) ((RelabelType *) node)->arg;
	return node;
}

/* Whether an expression holds a placeholder, which no batch child computes. */
static bool
has_placeholder(Node *node, void *context)
{
	if (node == NULL)
		return false;
	if (IsA(node, PlaceHolderVar))
		return true;
	return expression_tree_walker(node, has_placeholder, context);
}

/*
 * A key's operand: a column under binary coercions, or an expression of
 * the side's columns that the side's batch child computes as a target of
 * its own (a.k = b.k + 1, a.d = b.ts::date, lower(a.t) = lower(b.t)),
 * without a volatile function, a subplan or a placeholder, and of columns
 * no outer join below nulls. NULL for anything else.
 */
static Node *
key_operand(Node *node)
{
	Node	   *bare = strip_relabel(node);
	List	   *vars;

	if (plain_var(bare))
		return bare;
	if (contain_volatile_functions(node) || contain_subplans(node) ||
		has_placeholder(node, NULL))
		return NULL;
	vars = pull_var_clause(node, 0);
	if (vars == NIL)
		return NULL;
	foreach_node(Var, var, vars)
	{
		if (!bms_is_empty(var->varnullingrels))
			return NULL;
	}
	return node;
}

/*
 * The 64-bit hash function of a key a word does not hold: the clause is
 * the default equality of the type both operands are compared as, which
 * hashes.
 */
static Oid
key_hasher(const OpExpr *op, Oid left, Oid right)
{
	TypeCacheEntry *type;

	if (left != right)
		return InvalidOid;
	type = lookup_type_cache(left, TYPECACHE_EQ_OPR | TYPECACHE_HASH_EXTENDED_PROC);
	if (type->eq_opr != op->opno || !OidIsValid(type->hash_extended_proc))
		return InvalidOid;
	return type->hash_extended_proc;
}

/*
 * One clause as a key, when it is an equality of words of a column of each
 * side, or of values its type hashes, *hashed then set.
 */
static bool
add_key(RestrictInfo *rinfo, RelOptInfo *outerrel, RelOptInfo *innerrel,
		JoinKeys *keys, bool *hashed)
{
	Oid			hasher = InvalidOid;
	OpExpr	   *op;
	Node	   *outer_arg;
	Node	   *inner_arg;
	Node	   *outer;
	Node	   *inner;
	TessTableKeyKind outer_kind;
	TessTableKeyKind inner_kind;

	if (rinfo->pseudoconstant || !rinfo->can_join ||
		!OidIsValid(rinfo->hashjoinoperator) || !IsA(rinfo->clause, OpExpr))
		return false;
	op = (OpExpr *) rinfo->clause;
	set_opfuncid(op);
	if (bms_is_subset(rinfo->left_relids, outerrel->relids) &&
		bms_is_subset(rinfo->right_relids, innerrel->relids))
	{
		outer_arg = linitial(op->args);
		inner_arg = lsecond(op->args);
	}
	else if (bms_is_subset(rinfo->left_relids, innerrel->relids) &&
			 bms_is_subset(rinfo->right_relids, outerrel->relids))
	{
		outer_arg = lsecond(op->args);
		inner_arg = linitial(op->args);
	}
	else
		return false;
	/* The columns or expressions, the types those the equality compares. */
	outer = key_operand(outer_arg);
	inner = key_operand(inner_arg);
	if (outer == NULL || inner == NULL)
		return false;
	if (!word_equality(op->opfuncid) &&
		!OidIsValid(hasher = key_hasher(op, exprType(outer_arg), exprType(inner_arg))))
		return false;
	if (OidIsValid(hasher))
		outer_kind = inner_kind = TESS_TABLE_KEY_INT8;
	else if (!tess_word_key_kind(exprType(outer_arg), &outer_kind) ||
			 !tess_word_key_kind(exprType(inner_arg), &inner_kind))
		return false;
	keys->hashers = lappend_int(keys->hashers, (int) hasher);
	keys->collations = lappend_int(keys->collations, (int) op->inputcollid);
	*hashed = OidIsValid(hasher);
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
		bool		hashed = false;

		if (IS_OUTER_JOIN(jointype) && RINFO_IS_PUSHED_DOWN(rinfo, joinrel->relids))
		{
			if (!residual_supported(rinfo))
				return false;
			filters = lappend(filters, rinfo);
			continue;
		}
		if (keys->nkeys < TESS_TABLE_MAX_KEYS &&
			add_key(rinfo, outerrel, innerrel, keys, &hashed))
		{
			/* One hash for two values: the equality decides. */
			if (hashed)
				residual = lappend(residual, rinfo);
			continue;
		}
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
		foreach_ptr(Node, key, keys->inner)
		{
			if (IsA(key, Var) && var->varno == ((Var *) key)->varno &&
				var->varattno == ((Var *) key)->varattno)
				inner_keys = bms_add_member(inner_keys,
											foreach_current_index(key));
		}
	}
	/* Keys outside the target are columns of the scan tuple too, an expression's own. */
	foreach_ptr(Node, key, keys->inner)
	{
		if (!IsA(key, Var))
			*ninner += list_length(pull_var_clause(key, 0));
		else if (!bms_is_member(foreach_current_index(key), inner_keys))
			(*ninner)++;
	}
	/* So are the clauses' inner columns, counted generously. */
	foreach_node(Var, var, pull_var_clause((Node *) list_make2(keys->residual,
															   keys->filters), 0))
	{
		if (bms_is_member(var->varno, innerrel->relids))
			(*ninner)++;
	}
	return *ninner <= JOIN_MAX_INNER_COLUMNS;
}

/* Whether a node is the column. */
static bool
same_column(const Node *node, const Var *var)
{
	return IsA(node, Var) && ((const Var *) node)->varno == var->varno &&
		((const Var *) node)->varattno == var->varattno &&
		((const Var *) node)->varlevelsup == 0;
}

/* The operator of `outer op inner` for a key clause of the two columns. */
static Oid
outer_operator(const OpExpr *clause, const Var *outer)
{
	return same_column(strip_implicit_coercions(linitial(clause->args)), outer) ?
		clause->opno : get_commutator(clause->opno);
}

/* A column of a partitioned relation as its partition, a relation of the hierarchy, sees it. */
static Var *
partition_column(PlannerInfo *root, RelOptInfo *child, Var *var)
{
	AppendRelInfo *appinfo = root->append_rel_array[child->relid];

	return (Var *) adjust_appendrel_attrs(root, (Node *) var, 1, &appinfo);
}

/*
 * The partitioning of the relation or of a partitioned partition of it
 * whose key's first column is the column, compared by the operator's
 * family, and a hash one by that column alone: the core's pruning then
 * has steps for `column op value`, as it matches a clause to a key. NULL
 * where none is.
 */
static PartitionScheme
key_partitioning(PlannerInfo *root, RelOptInfo *rel, Var *column, Oid opno)
{
	PartitionScheme scheme = rel->part_scheme;
	int			part = -1;

	if (scheme == NULL || scheme->partnatts == 0)
		return NULL;
	if ((scheme->strategy != PARTITION_STRATEGY_HASH || scheme->partnatts == 1) &&
		op_in_opfamily(opno, scheme->partopfamily[0]))
	{
		foreach_ptr(Node, expr, rel->partexprs[0])
		{
			if (same_column(expr, column))
				return scheme;
		}
	}
	while ((part = bms_next_member(rel->live_parts, part)) >= 0)
	{
		RelOptInfo *child = rel->part_rels[part];
		PartitionScheme found;

		if (child == NULL || child->part_scheme == NULL)
			continue;
		found = key_partitioning(root, child, partition_column(root, child, column),
								 opno);
		if (found != NULL)
			return found;
	}
	return NULL;
}

/*
 * The key the join prunes its outer side by, at planning and at execution
 * alike: the first key of words whose outer column is a partition key
 * (key_partitioning), or -1; *scheme then that partitioning.
 */
static int
prune_key(PlannerInfo *root, RelOptInfo *rel, List *clauses, List *outer_keys,
		  List *hashers, PartitionScheme *scheme)
{
	for (int key = 0; key < list_length(outer_keys); key++)
	{
		OpExpr	   *clause = list_nth_node(OpExpr, clauses, key);
		Var		   *outer = list_nth(outer_keys, key);

		if (list_nth_int(hashers, key) != 0 || list_length(clause->args) != 2 ||
			!IsA(outer, Var))
			continue;
		*scheme = key_partitioning(root, rel, outer, outer_operator(clause, outer));
		if (*scheme != NULL)
			return key;
	}
	return -1;
}

/* `outer op value` for op of the strategy in the family of the partitioning, or NULL. */
static Expr *
bound_clause(PartitionScheme scheme, int strategy, Var *outer, Const *value)
{
	Oid			opno = get_opfamily_member(scheme->partopfamily[0],
										   scheme->partopcintype[0],
										   value->consttype, strategy);
	Expr	   *clause;

	if (!OidIsValid(opno))
		return NULL;
	clause = make_opclause(opno, BOOLOID, false, (Expr *) copyObject(outer),
						   (Expr *) copyObject(value), InvalidOid, value->constcollid);
	set_opfuncid((OpExpr *) clause);
	return clause;
}

/*
 * The clauses on the outer key the inner key's values keep to, as far as
 * the planner knows them before execution: between the lowest and the
 * highest value of its statistics (the histogram's ends and the common
 * values), and its relation's clauses `key op constant`, carried over to
 * the outer key, which the core does for no inequality. NIL where it knows
 * nothing.
 */
static List *
inner_key_bounds(PlannerInfo *root, PartitionScheme scheme, Var *outer, Var *inner)
{
	Oid			family = scheme->partopfamily[0];
	Oid			less = get_opfamily_member(family, inner->vartype, inner->vartype,
										   BTLessStrategyNumber);
	VariableStatData vardata;
	List	   *bounds = NIL;

	examine_variable(root, (Node *) inner, 0, &vardata);
	if (OidIsValid(less) && HeapTupleIsValid(vardata.statsTuple) && vardata.acl_ok)
	{
		FmgrInfo	compare;
		AttStatsSlot slot;
		bool		found = false;
		Datum		low = 0;
		Datum		high = 0;
		int16		typlen;
		bool		typbyval;

		fmgr_info(get_opcode(less), &compare);
		get_typlenbyval(inner->vartype, &typlen, &typbyval);
		/* The histogram is sorted by the type's <, which the family has. */
		if (get_attstatsslot(&slot, vardata.statsTuple, STATISTIC_KIND_HISTOGRAM, less,
							 ATTSTATSSLOT_VALUES))
		{
			if (slot.nvalues > 0)
			{
				low = datumCopy(slot.values[0], typbyval, typlen);
				high = datumCopy(slot.values[slot.nvalues - 1], typbyval, typlen);
				found = true;
			}
			free_attstatsslot(&slot);
		}
		if (get_attstatsslot(&slot, vardata.statsTuple, STATISTIC_KIND_MCV, InvalidOid,
							 ATTSTATSSLOT_VALUES))
		{
			for (int i = 0; i < slot.nvalues; i++)
			{
				Datum		value = slot.values[i];

				if (!found || DatumGetBool(FunctionCall2Coll(&compare, inner->varcollid,
															 value, low)))
					low = datumCopy(value, typbyval, typlen);
				if (!found || DatumGetBool(FunctionCall2Coll(&compare, inner->varcollid,
															 high, value)))
					high = datumCopy(value, typbyval, typlen);
				found = true;
			}
			free_attstatsslot(&slot);
		}
		if (found)
		{
			Expr	   *lower = bound_clause(scheme, BTGreaterEqualStrategyNumber, outer,
											 makeConst(inner->vartype, inner->vartypmod,
													   inner->varcollid, typlen, low,
													   false, typbyval));
			Expr	   *upper = bound_clause(scheme, BTLessEqualStrategyNumber, outer,
											 makeConst(inner->vartype, inner->vartypmod,
													   inner->varcollid, typlen, high,
													   false, typbyval));

			if (lower != NULL && upper != NULL)
				bounds = list_make2(lower, upper);
		}
	}
	ReleaseVariableStats(vardata);
	foreach_node(RestrictInfo, rinfo, find_base_rel(root, inner->varno)->baserestrictinfo)
	{
		OpExpr	   *op = (OpExpr *) rinfo->clause;
		Node	   *left;
		Node	   *right;
		Const	   *value;
		int			strategy;
		Expr	   *bound;

		if (!IsA(op, OpExpr) || list_length(op->args) != 2)
			continue;
		left = strip_implicit_coercions(linitial(op->args));
		right = strip_implicit_coercions(lsecond(op->args));
		strategy = get_op_opfamily_strategy(op->opno, family);
		if (same_column(left, inner) && IsA(right, Const))
			value = (Const *) right;
		else if (same_column(right, inner) && IsA(left, Const))
		{
			/* `value op key` is `key op' value`, op' the commuted strategy. */
			value = (Const *) left;
			strategy = BTMaxStrategyNumber + 1 - strategy;
		}
		else
			continue;
		if (value->constisnull || strategy < BTLessStrategyNumber ||
			strategy > BTMaxStrategyNumber)
			continue;
		bound = bound_clause(scheme, strategy, outer, value);
		if (bound != NULL)
			bounds = lappend(bounds, bound);
	}
	return bounds;
}

/*
 * The partitions of the relation that clauses on its columns leave, by the
 * core's pruning at planning, at each level: the leaves, and the
 * partitioned partitions with a leaf left.
 */
static Relids
leaves_left(PlannerInfo *root, RelOptInfo *rel, List *clauses)
{
	RelOptInfo	pruned = *rel;
	Relids		leaves = NULL;
	Bitmapset  *parts;
	int			part = -1;

	/* The core's pruning at planning reads the relation's own clauses. */
	pruned.baserestrictinfo = clauses;
	parts = bms_intersect(prune_append_rel_partitions(&pruned), rel->live_parts);
	while ((part = bms_next_member(parts, part)) >= 0)
	{
		RelOptInfo *child = rel->part_rels[part];

		if (child == NULL)
			continue;
		if (child->part_scheme != NULL)
		{
			AppendRelInfo *appinfo = root->append_rel_array[child->relid];
			Relids		below = leaves_left(root, child,
											(List *) adjust_appendrel_attrs(root,
																			(Node *) clauses,
																			1, &appinfo));

			if (below != NULL)
				leaves = bms_add_members(bms_add_members(leaves, below), child->relids);
		}
		else
			leaves = bms_add_members(leaves, child->relids);
	}
	return leaves;
}

/*
 * The partitions of the outer side the join's pruning is expected to
 * leave, as it prunes at execution (write_join_prune): an inner, semi or
 * right join over a partitioned relation, by the prune key and the bounds
 * the planner knows of the inner key's values. NULL where it expects to
 * prune none, or knows nothing.
 */
static Relids
expected_leaves(PlannerInfo *root, RelOptInfo *outerrel, const JoinKeys *keys,
				JoinType jointype)
{
	PartitionScheme scheme = NULL;
	int			key;
	List	   *bounds;
	Relids		leaves;

	if (!enable_partition_pruning || !IS_SIMPLE_REL(outerrel) ||
		outerrel->part_scheme == NULL ||
		(jointype != JOIN_INNER && jointype != JOIN_SEMI && jointype != JOIN_RIGHT))
		return NULL;
	key = prune_key(root, outerrel, keys->clauses, keys->outer, keys->hashers, &scheme);
	if (key < 0)
		return NULL;
	/* The bounds of the inner side's column; an expression has none known. */
	if (!IsA(list_nth(keys->inner, key), Var))
		return NULL;
	bounds = inner_key_bounds(root, scheme, list_nth_node(Var, keys->outer, key),
							  list_nth_node(Var, keys->inner, key));
	if (bounds == NIL)
		return NULL;
	leaves = leaves_left(root, outerrel, bounds);
	/* Every partition pruned: a set no relation is in, NULL being no estimate. */
	return leaves != NULL ? leaves : bms_make_singleton(0);
}

/*
 * The core's Append path of the outer side over the partitions expected
 * to be left, with its cost and rows by the core's costing, for the
 * template's cost only: the join's child keeps every partition, which it
 * prunes at execution. The path itself where it is no Append.
 */
static Path *
pruned_append(PlannerInfo *root, Path *path, Relids leaves)
{
	AppendPath *append = (AppendPath *) path;
	AppendPath *pruned;

	if (leaves == NULL || !IsA(path, AppendPath))
		return path;
	pruned = makeNode(AppendPath);
	*pruned = *append;
	pruned->subpaths = NIL;
	pruned->first_partial_path = 0;
	foreach_ptr(Path, subpath, append->subpaths)
	{
		if (!bms_is_subset(subpath->parent->relids, leaves))
			continue;
		pruned->subpaths = lappend(pruned->subpaths, subpath);
		if (foreach_current_index(subpath) < append->first_partial_path)
			pruned->first_partial_path++;
	}
	cost_append(pruned, root);
	pruned->path.rows = clamp_row_est(pruned->path.rows);
	return &pruned->path;
}

/*
 * The outer side's parallel Append that divides every partition among
 * the participants, over the partitions' cheapest partial paths, as the
 * core builds it before its add_partial_path may keep instead the Append
 * that gives some partitions whole to one participant each: our partial
 * scan's cost carries a worker's start (scan_planner.c), which the core's
 * Append adds up once a partition. The path itself where it divides
 * every partition already; NULL where a partition has no partial path.
 */
static Path *
divided_append(PlannerInfo *root, RelOptInfo *rel, AppendPath *append)
{
	AppendPathInput input = {0};

	if (append->first_partial_path == 0)
		return &append->path;
	foreach_ptr(Path, subpath, append->subpaths)
	{
		RelOptInfo *child = subpath->parent;

		if (foreach_current_index(subpath) >= append->first_partial_path)
			input.partial_subpaths = lappend(input.partial_subpaths, subpath);
		else if (child->part_scheme == NULL && child->partial_pathlist != NIL)
			input.partial_subpaths = lappend(input.partial_subpaths,
											 linitial(child->partial_pathlist));
		else
			return NULL;
	}
	input.child_append_relid_sets = append->child_append_relid_sets;
	return (Path *) create_append_path(root, rel, input, NIL, NULL,
									   append->path.parallel_workers, true, -1);
}

/*
 * A batch child with the join's key expressions among its targets, which
 * it computes as it projects (a copy, the child left as it is); the child
 * itself where every key is a column or a target already; NULL where it
 * does not project.
 */
static Path *
with_key_targets(PlannerInfo *root, Path *child, List *keys)
{
	List	   *missing = NIL;
	CustomPath *copy;
	PathTarget *target;

	foreach_ptr(Node, key, keys)
	{
		if (!IsA(key, Var) && !list_member(child->pathtarget->exprs, key))
			missing = lappend(missing, key);
	}
	if (missing == NIL)
		return child;
	if (!IsA(child, CustomPath) ||
		(((CustomPath *) child)->flags & CUSTOMPATH_SUPPORT_PROJECTION) == 0)
		return NULL;
	copy = makeNode(CustomPath);
	memcpy(copy, child, sizeof(CustomPath));
	target = copy_pathtarget(child->pathtarget);
	foreach_ptr(Node, key, missing)
		add_new_column_to_pathtarget(target, (Expr *) key);
	set_pathtarget_cost_width(root, target);
	copy->path.pathtarget = target;
	copy->path.total_cost += (target->cost.per_tuple - child->pathtarget->cost.per_tuple) *
		child->rows;
	return &copy->path;
}

/* The rows of a batch, as the sources fill them. */
#define JOIN_COST_BATCH_ROWS 64

/*
 * The path's cost: the children's as they are, and the node's own time in
 * the units of the scan model (docs/nodes.md, Parameters), from the rows
 * the planner expects of each side and of the join. A row built, a row
 * probed, a pair, and a batch published (an outer batch with a pair goes
 * out as a round, which the parent pays for however few rows it selects);
 * an inner column kept, gathered for a pair, a text value copied into the
 * table and gathered; a hashed key hashed on a row of either side; a pair
 * of a compact batch where the inner side has several records a key (the
 * inner keys' distinct values, from the statistics); the residual clauses
 * over the records matched, at the filter's price of a batch clause; a
 * Bloom filter's test a probe row in place of the probe of each row it
 * rejects, where the node is expected to build one (fewer probe rows with
 * a pair than tessera.join_bloom_ratio, a table of JOIN_BLOOM_MIN_ROWS
 * rows at least); and a price a row of either side once the table
 * outgrows hash_mem, every participant's with a shared table. A partial
 * path's rows are one participant's, and its cost that participant's
 * time. The outer child's cost is scaled by outer_scale, below 1 where
 * the join prunes its partitions.
 *
 * The time goes into the cost at tessera.join_cost_unit units a unit: the
 * core's hash join, which the planner weighs this path against, is costed
 * by the core's nominal constants, at 4.9 µs of its time a unit of its
 * cost over the same joins, where a unit of the scan model is 0.27 µs (the
 * core's rate is 1 to 7.5 µs by the join's shape: it underprices its
 * build the most). One rate keeps the node's joins ordered by their time
 * among themselves, and the node's join below the core's wherever it is
 * at least 1.5 times as fast, which the slowest shape measured is by 2.7.
 * Calibrated by bench/pg/joincost.
 */
static void
join_cost(PlannerInfo *root, RelOptInfo *joinrel, RelOptInfo *innerrel, JoinType jointype,
		  bool inner_unique, const JoinKeys *keys, const Path *outer, const Path *inner,
		  double outer_scale, bool shared, Path *path)
{
	double		probe_rows = outer->rows;
	double		build_rows = inner->rows;
	double		participants = shared ? tess_parallel_divisor(inner) : 1.0;
	double		table_rows = build_rows * participants;
	double		rows = path->rows;
	double		per_key = 1.0;
	double		matched;
	double		pairs;
	double		share;
	double		out_share;
	double		batches;
	bool		compact;
	bool		bloom;
	bool		spills;
	int			hashed = 0;
	int			ints = 0;
	int			texts = 0;
	List	   *kept = NIL;
	List	   *vars;
	Cost		build;
	Cost		probe;

	foreach_int(hasher, keys->hashers)
		hashed += hasher != 0;
	/* The inner columns kept in the records: the target's and the residual clauses'. */
	vars = list_copy(joinrel->reltarget->exprs);
	vars = list_concat(vars, pull_var_clause((Node *) keys->residual, 0));
	foreach_ptr(Node, node, vars)
	{
		Var		   *var = (Var *) node;
		int16		len;
		bool		byval;

		if (!IsA(node, Var) || !bms_is_member(var->varno, innerrel->relids) ||
			list_member(kept, var))
			continue;
		kept = lappend(kept, var);
		get_typlenbyval(var->vartype, &len, &byval);
		if (byval)
			ints++;
		else
			texts++;
	}
	/* Records a key: a compact batch of the pairs when there are several. */
	if (!inner_unique && keys->inner != NIL)
	{
		double		distinct = estimate_num_groups(root, keys->inner, table_rows, NULL, NULL);

		if (distinct > 0)
			per_key = Max(1.0, table_rows / distinct);
	}
	compact = per_key > 1.05;
	switch (jointype)
	{
		case JOIN_SEMI:
			matched = rows;
			pairs = rows;
			break;
		case JOIN_ANTI:
			/* The rows returned found no record; the others a pair each. */
			matched = Max(probe_rows - rows, 0);
			pairs = matched;
			break;
		case JOIN_LEFT:
		case JOIN_FULL:
			matched = Min(rows, probe_rows);
			pairs = rows;
			break;
		default:
			matched = compact ? rows / per_key : rows;
			pairs = rows;
			break;
	}
	matched = Min(matched, probe_rows);
	share = probe_rows > 0 ? matched / probe_rows : 0;
	if (jointype == JOIN_ANTI)
		out_share = probe_rows > 0 ? Min(rows / probe_rows, 1.0) : 0;
	else if (jointype == JOIN_LEFT || jointype == JOIN_FULL)
		out_share = 1.0;
	else
		out_share = share;
	batches = compact ? pairs / JOIN_COST_BATCH_ROWS :
		probe_rows / JOIN_COST_BATCH_ROWS * (1.0 - pow(1.0 - out_share, JOIN_COST_BATCH_ROWS));
	bloom = tess_join_bloom_ratio > 0 && table_rows >= JOIN_BLOOM_MIN_ROWS &&
		(tess_join_bloom_ratio >= 1.0 || share < tess_join_bloom_ratio);
	/*
	 * A record: 16 bytes of header, 8 a key, 8 for the NULL bits of the
	 * columns kept and 8 a column, and 8 of the index.
	 */
	spills = table_rows * (16 + 8 * keys->nkeys + 8 * (1 + ints + texts) + 8) >
		(double) get_hash_memory_limit() * participants;

	build = build_rows * (tess_join_build_cost + texts * tess_join_text_value_cost +
						  hashed * tess_join_hashed_key_cost);
	probe = probe_rows * (hashed * tess_join_hashed_key_cost +
						  (bloom ? tess_join_bloom_test_cost : tess_join_probe_cost)) +
		(bloom ? matched * tess_join_probe_cost : 0) +
		pairs * (tess_join_pair_cost + ints * tess_join_gather_cost +
				 texts * tess_join_text_value_cost +
				 (compact ? tess_join_compact_pair_cost : 0)) +
		batches * tess_join_batch_cost +
		matched * list_length(keys->residual) * tess_filter_clause_cost;
	if (spills)
	{
		build += build_rows * tess_join_spill_row_cost;
		probe += probe_rows * tess_join_spill_row_cost;
	}
	build /= tess_join_cost_unit;
	probe /= tess_join_cost_unit;
	path->startup_cost = outer->startup_cost * outer_scale + inner->total_cost + build;
	path->total_cost = outer->total_cost * outer_scale + inner->total_cost + build + probe;
}

/*
 * The path: the core's hash join of the same inputs as the template, for
 * its rows and properties, over batch paths of them, at the cost of
 * join_cost. Where the join prunes its outer TessAppend, the template
 * reads only the partitions expected to be left (leaves, expected_leaves),
 * and the outer child's cost is scaled down by as much.
 */
static CustomPath *
make_join_path(PlannerInfo *root, RelOptInfo *joinrel, JoinType jointype,
			   JoinPathExtraData *extra, const JoinKeys *keys,
			   Path *outer_path, Path *inner_path, bool shared, Relids leaves)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	JoinCostWorkspace workspace;
	List	   *hashclauses = keys->rinfos;
	HashPath   *template;
	Path	   *outer;
	Path	   *inner;
	Path	   *priced;

	/* A partial inner path's rows are one participant's share. */
	double		inner_rows = shared ?
		inner_path->rows * tess_parallel_divisor(inner_path) : inner_path->rows;

	outer = tess_batch_input_path(root, outer_path);
	inner = tess_batch_input_path(root, inner_path);
	/* A key that is an expression: its side's child computes it. */
	if (outer != NULL)
		outer = with_key_targets(root, outer, keys->outer);
	if (inner != NULL)
		inner = with_key_targets(root, inner, keys->inner);
	if (outer == NULL || inner == NULL)
		return NULL;
	priced = tess_path_node(outer) == &tess_append_node ?
		pruned_append(root, outer_path, leaves) : outer_path;
	initial_cost_hashjoin(root, &workspace, jointype, hashclauses,
						  priced, inner_path, extra, shared);
	/*
	 * The clauses' sides for this join, which the core's costing reads: a
	 * field of the join being costed, which the core sets the same way
	 * before it costs each one (clause_sides_match_join).
	 */
	foreach_node(RestrictInfo, rinfo, keys->rinfos)
		rinfo->outer_is_left = bms_is_subset(rinfo->left_relids,
											 outer_path->parent->relids);
	template = create_hashjoin_path(root, joinrel, jointype, &workspace,
									extra, priced, inner_path, shared,
									extra->restrictlist, NULL, hashclauses);
	join_cost(root, joinrel, inner_path->parent, jointype, extra->inner_unique, keys,
			  outer, inner,
			  outer_path->total_cost > 0 ? priced->total_cost / outer_path->total_cost : 1.0,
			  shared, &template->jpath.path);
	config.template_path = &template->jpath.path;
	config.methods = &join_path_methods;
	config.node = &tess_hash_join_node;
	config.children = list_make2(outer, inner);
	/* A target above the join becomes the node's, computed over the pairs. */
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	config.expressions = list_make5(keys->clauses, keys->outer, keys->inner,
									keys->residual, keys->filters);
	config.node_data = (Node *) list_make5(keys->outer_kinds, keys->inner_kinds,
										   list_make4_int(extra->inner_unique ? 1 : 0,
														  (int) Min(inner_rows,
																	(double) PG_INT32_MAX),
														  (int) jointype,
														  shared ? 1 : 0),
										   keys->hashers, keys->collations);
	return tess_path_create(&config);
}

/*
 * The partial paths over a partial outer path, every participant building
 * the whole inner side, as the core's hash join without a shared table
 * does, from the cheapest inner path a worker may run; and a shared
 * table, where the core offers a Parallel Hash: the inner side's partial
 * path divides the build among the participants too, into one table in
 * the query's shared memory. The paths are parallel-aware for the
 * counters the node shares. RIGHT and FULL take a shared table only, as
 * the core does: with a table each, every participant would return the
 * records without a pair.
 */
static void
add_partial_join_paths(PlannerInfo *root, RelOptInfo *joinrel, RelOptInfo *innerrel,
					   JoinType jointype, JoinPathExtraData *extra, const JoinKeys *keys,
					   Path *outer_path, Path *inner_path, Relids leaves)
{
	CustomPath *path;

	if (jointype == JOIN_INNER || jointype == JOIN_SEMI)
	{
		Path	   *filtered = tess_filter_row_path(root, outer_path->parent, outer_path);

		if (filtered != NULL)
			outer_path = filtered;
	}
	path = jointype == JOIN_RIGHT || jointype == JOIN_FULL ? NULL :
		make_join_path(root, joinrel, jointype, extra, keys,
					   outer_path, inner_path, false, leaves);
	if (path != NULL && path->path.parallel_safe && path->path.parallel_workers > 0)
	{
		path->path.parallel_aware = true;
		add_partial_path(joinrel, &path->path);
	}
	if (!enable_parallel_hash || innerrel->partial_pathlist == NIL)
		return;
	path = make_join_path(root, joinrel, jointype, extra, keys,
						  outer_path, linitial(innerrel->partial_pathlist), true, leaves);
	if (path == NULL || !path->path.parallel_safe ||
		path->path.parallel_workers <= 0)
		return;
	path->path.parallel_aware = true;
	add_partial_path(joinrel, &path->path);
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
	Relids		leaves;

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
	leaves = expected_leaves(root, outerrel, &keys, jointype);
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
						  outer_path, inner_path, false, leaves);
	if (path != NULL)
		add_path(joinrel, &path->path);

	/*
	 * Under a Gather: the outer side's cheapest partial path divides the
	 * rows. A join expected to prune its partitions divides each among
	 * the participants: the core's Append may give a partition whole to
	 * one participant, which leaves the others nothing to read once fewer
	 * partitions are left than participants; with at least as many left,
	 * the core's Append competes, the divided one's cost counting a
	 * worker's start once a partition.
	 */
	if (!joinrel->consider_parallel || outerrel->partial_pathlist == NIL ||
		!bms_is_empty(joinrel->lateral_relids))
		return;
	if (!inner_path->parallel_safe)
		inner_path = get_cheapest_parallel_safe_total_inner(innerrel->pathlist);
	if (inner_path == NULL)
		return;
	outer_path = linitial(outerrel->partial_pathlist);
	if (leaves != NULL && IsA(outer_path, AppendPath))
	{
		Path	   *divided = divided_append(root, outerrel, (AppendPath *) outer_path);
		AppendPath *left = (AppendPath *) pruned_append(root, outer_path, leaves);
		int			participants = outer_path->parallel_workers +
			(parallel_leader_participation ? 1 : 0);

		if (divided != NULL && divided != outer_path)
		{
			add_partial_join_paths(root, joinrel, innerrel, jointype, extra, &keys,
								   divided, inner_path, leaves);
			if (list_length(left->subpaths) < participants)
				return;
		}
	}
	add_partial_join_paths(root, joinrel, innerrel, jointype, extra, &keys,
						   outer_path, inner_path, leaves);
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


/*
 * The batch column of a child that holds this column or computes this
 * key. A column matches by its relation and attribute alone: above an
 * outer join it carries the join's nulling relations, which the child's
 * own target does not.
 */
static int
child_column(const TessPlanChild *child, const Node *node)
{
	const Var  *var = (const Var *) node;
	int			column = -1;

	if (!IsA(node, Var))
		column = tess_plan_child_column(child, (Expr *) node);
	else
		foreach_ptr(TargetEntry, entry, child->plan->targetlist)
		{
			Var		   *other = (Var *) entry->expr;

			if (IsA(other, Var) && other->varno == var->varno &&
				other->varattno == var->varattno &&
				other->varlevelsup == var->varlevelsup)
			{
				column = tess_layout_column(&child->layout, entry->resno - 1);
				break;
			}
		}
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
/* A parameter of execution of the inner key's type, which the join sets. */
static Param *
prune_param(PlannerInfo *root, Node *inner)
{
	Param	   *param = makeNode(Param);

	param->paramkind = PARAM_EXEC;
	param->paramid = assign_special_exec_param(root);
	param->paramtype = exprType(inner);
	param->paramtypmod = exprTypmod(inner);
	param->paramcollid = exprCollation(inner);
	param->location = -1;
	return param;
}

/* outer op param, op an operator of the given strategy of the family, or NULL. */
static Expr *
prune_clause(Oid opno, Var *outer, Param *param)
{
	Expr	   *clause;

	if (!OidIsValid(opno))
		return NULL;
	clause = make_opclause(opno, BOOLOID, false, (Expr *) copyObject(outer), (Expr *) param,
						   InvalidOid, param->paramcollid);
	set_opfuncid((OpExpr *) clause);
	return clause;
}

/*
 * The core's pruning description of the relation's partitions for
 * clauses, as prune_info of append.c makes it, taken off the planner's
 * list: setrefs registers the descriptions of Appends alone, so the node
 * keeps it in its plan data and makes its state itself (TessAppend's
 * prune_start, plan 4.30); NULL
 * where the clauses prune none.
 */
static PartitionPruneInfo *
prune_description(PlannerInfo *root, RelOptInfo *rel, List *children, List *clauses)
{
	int			index = make_partition_pruneinfo(root, rel, children, clauses);
	PartitionPruneInfo *info;

	if (index < 0)
		return NULL;
	info = llast_node(PartitionPruneInfo, root->partPruneInfos);
	if (index != list_length(root->partPruneInfos) - 1 ||
		!bms_equal(info->relids, rel->relids))
		elog(ERROR, "TessHashJoin found another pruning description than its own");
	root->partPruneInfos = list_delete_last(root->partPruneInfos);
	return info;
}

/*
 * Pruning of a partitioned outer side by the inner side's keys: where the
 * outer path is TessAppend over a partitioned relation, the join keeps no
 * outer row without a pair (INNER, SEMI, RIGHT) and a key of words has the
 * partition key on its outer side, the core's pruning steps for
 * `key = $p` and for `key >= $lo AND key <= $hi` (where the key's btree
 * family compares the two types), for the key prune_key chooses, as the
 * planning of the path expects (expected_leaves), over parameters of execution the join
 * sets from its keys once built (docs/nodes.md). No change of the core's
 * pruning, which takes no column of another relation: the parameters stand
 * for the inner side's values. Its plan data: the key's number, the three
 * parameters and the two descriptions; -1 as the key where none prunes.
 */
static void
write_join_prune(TessPlanWriter *writer, PlannerInfo *root, const TessPlanChild *outer,
				 List *hash_clauses, List *outer_keys, List *inner_keys, List *hashers,
				 JoinType jointype)
{
	RelOptInfo *rel = outer->path->parent;
	PartitionScheme scheme = NULL;
	int			key = -1;

	if (enable_partition_pruning && tess_path_node(outer->path) == &tess_append_node &&
		IS_SIMPLE_REL(rel) && rel->part_scheme != NULL &&
		(jointype == JOIN_INNER || jointype == JOIN_SEMI || jointype == JOIN_RIGHT))
		key = prune_key(root, rel, hash_clauses, outer_keys, hashers, &scheme);
	if (key >= 0)
	{
		List	   *children = ((CustomPath *) outer->path)->custom_paths;
		OpExpr	   *clause = list_nth_node(OpExpr, hash_clauses, key);
		Var		   *outer_var = copyObject(list_nth_node(Var, outer_keys, key));
		Node	   *inner_var = list_nth(inner_keys, key);
		Param	   *value = prune_param(root, inner_var);
		Param	   *low = prune_param(root, inner_var);
		Param	   *high = prune_param(root, inner_var);
		Oid			family = scheme->partopfamily[0];
		Oid			type = scheme->partopcintype[0];
		Expr	   *lower;
		Expr	   *upper;
		PartitionPruneInfo *values;
		PartitionPruneInfo *range = NULL;

		/* The partition key as the relation's scan sees it, below any outer join. */
		outer_var->varnullingrels = NULL;
		values = prune_description(root, rel, children,
								   list_make1(prune_clause(outer_operator(clause, outer_var),
														   outer_var, value)));
		if (values != NULL)
		{
			lower = prune_clause(get_opfamily_member(family, type, exprType(inner_var),
													 BTGreaterEqualStrategyNumber),
								 outer_var, low);
			upper = prune_clause(get_opfamily_member(family, type, exprType(inner_var),
													 BTLessEqualStrategyNumber),
								 outer_var, high);
			if (lower != NULL && upper != NULL)
				range = prune_description(root, rel, children, list_make2(lower, upper));
			tess_plan_write_int(writer, "prune_key", key);
			tess_plan_write_int_list(writer, "prune_params",
									 list_make3_int(value->paramid, low->paramid,
													high->paramid));
			tess_plan_write_node(writer, "prune_values", (Node *) values);
			tess_plan_write_node(writer, "prune_range", (Node *) range);
			return;
		}
	}
	tess_plan_write_int(writer, "prune_key", -1);
	tess_plan_write_int_list(writer, "prune_params", NIL);
	tess_plan_write_node(writer, "prune_values", NULL);
	tess_plan_write_node(writer, "prune_range", NULL);
}

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
		foreach_ptr(Node, key, side == 0 ? outer_keys : inner_keys)
			foreach_node(Var, var, pull_var_clause(key, 0))
				wanted = lappend(wanted, makeTargetEntry((Expr *) var, 0, NULL, true));
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
								  child_column(side == 0 ? &outer : &inner, (Node *) var));
		}
	}
	layout.ncolumns = list_length(scan);
	layout.ntargets = list_length(scan);

	writer = tess_plan_writer_create(TESS_HASH_JOIN_DATA,
									 TESS_HASH_JOIN_DATA_VERSION);
	tess_plan_write_int_list(writer, "sides", sides);
	tess_plan_write_int_list(writer, "child_columns", columns);
	foreach_ptr(Node, key, outer_keys)
		outer_columns = lappend_int(outer_columns, child_column(&outer, key));
	foreach_ptr(Node, key, inner_keys)
		inner_columns = lappend_int(inner_columns, child_column(&inner, key));
	tess_plan_write_int_list(writer, "outer_keys", outer_columns);
	tess_plan_write_int_list(writer, "inner_keys", inner_columns);
	tess_plan_write_int_list(writer, "outer_kinds", linitial(data));
	tess_plan_write_int_list(writer, "inner_kinds", lsecond(data));
	tess_plan_write_int_list(writer, "key_hashers", list_nth(data, 3));
	tess_plan_write_int_list(writer, "key_collations", list_nth(data, 4));
	tess_plan_write_int_list(writer, "residual_batch", residual_batch);
	tess_plan_write_int_list(writer, "filter_batch", filter_batch);
	tess_plan_write_int(writer, "jointype", lthird_int(lthird(data)));
	tess_plan_write_int(writer, "inner_unique", linitial_int(lthird(data)));
	tess_plan_write_int(writer, "inner_rows", lsecond_int(lthird(data)));
	tess_plan_write_int(writer, "shared", list_length(lthird(data)) > 3 ?
						lfourth_int(lthird(data)) : 0);
	write_join_prune(writer, root, &outer, hash_clauses, outer_keys, inner_keys,
					 list_nth(data, 3), (JoinType) lthird_int(lthird(data)));

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

#include "postgres.h"

#include "nodes/nodeFuncs.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "optimizer/planner.h"
#include "optimizer/tlist.h"
#include "utils/lsyscache.h"

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * The planner of TessSort: the sort keys the node takes, the path that
 * stands in for the core's Sort, and the plan it writes for the executor
 * (sort.c). See docs/nodes.md.
 */

static create_upper_paths_hook_type previous_create_upper_paths_hook = NULL;

static Plan *sort_plan(PlannerInfo *root, RelOptInfo *rel,
					   CustomPath *best_path, List *tlist, List *clauses,
					   List *custom_plans);

static const CustomPathMethods sort_path_methods = {
	.CustomName = "TessSort",
	.PlanCustomPath = sort_plan,
};

/*
 * The place in the target of the expression a path key orders by, with
 * its kind and flags: an int4 or int8 of the integer operator family,
 * ascending or descending. False for any other key.
 */
bool
tess_sort_key_of(PathKey *pathkey, PathTarget *target, Relids relids, int *place,
			TessSortKey *key)
{
	EquivalenceClass *ec = pathkey->pk_eclass;

	if (ec->ec_has_volatile ||
		(pathkey->pk_cmptype != COMPARE_LT && pathkey->pk_cmptype != COMPARE_GT))
		return false;
	foreach_ptr(Expr, expr, target->exprs)
	{
		Oid			type = exprType((Node *) expr);
		TessTableKeyKind kind;

		if (!tess_word_key_order(type, pathkey->pk_opfamily) ||
			!tess_word_key_kind(type, &kind) ||
			find_ec_member_matching_expr(ec, expr, relids) == NULL)
			continue;
		*place = foreach_current_index(expr);
		key->kind = kind;
		key->flags = (pathkey->pk_cmptype == COMPARE_GT ? TESS_SORT_DESCENDING : 0) |
			(pathkey->pk_nulls_first ? TESS_SORT_NULLS_FIRST : 0);
		return true;
	}
	return false;
}

/*
 * A key the kernels do not order by words: its expression's place in the
 * target, the operator that orders it (of the path key's operator family,
 * < or >) and its collation. False for a volatile key or a type without
 * one.
 */
bool
tess_sort_generic_key(PathKey *pathkey, PathTarget *target, Relids relids, int *place,
					  Oid *sortop, Oid *collation)
{
	EquivalenceClass *ec = pathkey->pk_eclass;

	if (ec->ec_has_volatile ||
		(pathkey->pk_cmptype != COMPARE_LT && pathkey->pk_cmptype != COMPARE_GT))
		return false;
	foreach_ptr(Expr, expr, target->exprs)
	{
		EquivalenceMember *member = find_ec_member_matching_expr(ec, expr, relids);

		if (member == NULL)
			continue;
		/* The family's type, as varchar is compared as text. */
		*sortop = get_opfamily_member_for_cmptype(pathkey->pk_opfamily, member->em_datatype,
												  member->em_datatype, pathkey->pk_cmptype);
		if (!OidIsValid(*sortop))
			continue;
		*place = foreach_current_index(expr);
		*collation = ec->ec_collation;
		return true;
	}
	return false;
}

/*
 * The node's path in place of the core's full sort: the same planner
 * properties over the batch child of the sort's input, with the key
 * expressions, which the sort's targets hold, and their kinds and flags. NULL when a key is not one the
 * kernels sort, the output has too many columns, or the input cannot be
 * read in batches.
 */
static CustomPath *
make_sort_path(PlannerInfo *root, SortPath *sort)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *input = sort->subpath;
	PathTarget *target = input->pathtarget;
	List	   *exprs = NIL;
	List	   *kinds = NIL;
	List	   *flags = NIL;
	List	   *sortops = NIL;
	List	   *collations = NIL;
	int			nkeys = list_length(sort->path.pathkeys);
	int			ncolumns = list_length(target->exprs);
	Path	   *child;

	if (nkeys == 0 || nkeys > TESS_TABLE_MAX_KEYS ||
		ncolumns == 0 || ncolumns > TESS_ROWS_MAX_COLUMNS)
		return NULL;
	foreach_node(PathKey, pathkey, sort->path.pathkeys)
	{
		TessSortKey key;
		int			place;
		Oid			sortop = InvalidOid;
		Oid			collation = InvalidOid;

		/*
		 * A key of another type orders by its type's comparison (sort
		 * support): the kernels order by its abbreviated key where it has
		 * one, and equal words by the comparison.
		 */
		if (tess_sort_key_of(pathkey, target, input->parent->relids, &place, &key))
		{
			/* Its comparison, for the rows a generic key before it leaves equal. */
			Oid			type = exprType(list_nth(target->exprs, place));

			sortop = get_opfamily_member_for_cmptype(pathkey->pk_opfamily, type, type,
													 pathkey->pk_cmptype);
			collation = pathkey->pk_eclass->ec_collation;
		}
		else if (tess_sort_generic_key(pathkey, target, input->parent->relids, &place,
								  &sortop, &collation))
		{
			/*
			 * A first key without an abbreviated key (float8, text under a
			 * collation of libc): every row one group, which the node
			 * orders by the comparison, as the core's sort does, and was
			 * measured no slower over the same scan (a million rows: text
			 * 2712 ms against 2640, float8 90.7 against 89.1, float8 and
			 * an integer 125.0 against 92.6); with a key before it, groups
			 * of that key's values. Under a limit, a type passed by value
			 * compares cheaply, and the core's bounded heap of tuples
			 * stays ahead of the node's (float8, LIMIT 10: 10.5 ms against
			 * 16.5; text 132.7 against 117.0 the other way): the core's.
			 */
			if (foreach_current_index(pathkey) == 0 && root->limit_tuples >= 0 &&
				get_typbyval(exprType(list_nth(target->exprs, place))))
				return NULL;
			key.kind = TESS_SORT_KIND_GENERIC;
			key.flags = (pathkey->pk_cmptype == COMPARE_GT ? TESS_SORT_DESCENDING : 0) |
				(pathkey->pk_nulls_first ? TESS_SORT_NULLS_FIRST : 0);
		}
		else
			return NULL;
		exprs = lappend(exprs, list_nth(target->exprs, place));
		kinds = lappend_int(kinds, (int) key.kind);
		flags = lappend_int(flags, (int) key.flags);
		sortops = lappend_int(sortops, (int) sortop);
		collations = lappend_int(collations, (int) collation);
	}
	/* Rows past work_mem go to runs on disk and merge: no gate on the rows. */
	child = tess_batch_input_path(root, input);
	if (child == NULL)
		return NULL;
	config.template_path = &sort->path;
	config.methods = &sort_path_methods;
	config.node = &tess_sort_node;
	config.children = list_make1(child);
	config.expressions = exprs;
	config.node_data = (Node *) list_make4(kinds, flags, sortops, collations);
	config.flags = CUSTOMPATH_SUPPORT_BACKWARD_SCAN;
	return tess_path_create(&config);
}

/*
 * The node's path in place of each of the core's full sorts of the
 * ordered relation, also one under a projection. Under LIMIT the limit
 * sets the node a bound at execution and it keeps the best rows in a heap
 * (top-N); WITH TIES, which passes no bound, keeps the core's sort, as
 * does a backend without the kernels module.
 */
static void
create_upper_paths(PlannerInfo *root, UpperRelationKind stage,
				   RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra)
{
	ListCell   *lc;
	const TessKernelOps *kernels;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel,
										 extra);
	if (!*tess_runtime_api()->settings->enable || stage != UPPERREL_ORDERED ||
		root->parse->limitOption == LIMIT_OPTION_WITH_TIES)
		return;
	/* Without the kernels module there is nothing to sort with. */
	kernels = tess_runtime_kernels();
	if (kernels == NULL || !TESS_ABI_HAS_FIELD(kernels, TessKernelOps, sort))
		return;
	foreach(lc, output_rel->pathlist)
	{
		Path	   *path = lfirst(lc);
		ProjectionPath *projection = NULL;
		CustomPath *sort;

		if (IsA(path, ProjectionPath))
		{
			projection = (ProjectionPath *) path;
			path = projection->subpath;
		}
		/*
		 * A sort in every participant under a Gather Merge: the node sorts
		 * each participant's share, the Gather Merge merges them as it
		 * merges the core's sorts.
		 */
		if (IsA(path, GatherMergePath))
		{
			GatherMergePath *gather = (GatherMergePath *) path;
			ProjectionPath *below = NULL;
			Path	   *subpath = gather->subpath;

			if (IsA(subpath, ProjectionPath))
			{
				below = (ProjectionPath *) subpath;
				subpath = below->subpath;
			}
			if (!IsA(subpath, SortPath))
				continue;
			sort = make_sort_path(root, (SortPath *) subpath);
			if (sort == NULL || !sort->path.parallel_safe)
				continue;
			/* Parallel-aware for the counters the participants share. */
			sort->path.parallel_aware = true;
			if (below != NULL)
				below->subpath = &sort->path;
			else
				gather->subpath = &sort->path;
			continue;
		}
		if (!IsA(path, SortPath))
			continue;
		sort = make_sort_path(root, (SortPath *) path);
		if (sort == NULL)
			continue;
		/*
		 * Replace the path in place rather than through add_path: nothing is
		 * freed, and the sort's input lives on in the input relation.
		 */
		if (projection != NULL)
			projection->subpath = &sort->path;
		else
			lfirst(lc) = sort;
	}

	/*
	 * The node's sort of the cheapest partial path under TessGatherMerge,
	 * at the batch gather's cost of a row: the core's Gather Merge of the
	 * same sort, costed at its own, may have lost to a serial sort already.
	 */
	if (output_rel->consider_parallel && root->sort_pathkeys != NIL &&
		input_rel->partial_pathlist != NIL && output_rel->pathlist != NIL)
	{
		Path	   *input = linitial(input_rel->partial_pathlist);
		CustomPath *sort;
		Path	   *path;

		/* make_sort_path reads a core scan through the node's batch input. */
		if (pathkeys_contained_in(root->sort_pathkeys, input->pathkeys))
			return;
		sort = make_sort_path(root, create_sort_path(root, output_rel, input,
													 root->sort_pathkeys,
													 root->limit_tuples));
		if (sort == NULL || !sort->path.parallel_safe)
			return;
		sort->path.parallel_aware = true;
		/* Every path of the ordered relation emits its target; the relation keeps none. */
		path = tess_gather_merge_path(root, output_rel, &sort->path,
									  ((Path *) linitial(output_rel->pathlist))->pathtarget);
		if (path != NULL)
			add_path(output_rel, path);
	}
}

/*
 * The output columns are the plan's targets, each a column of the child's
 * target list; the keys are targets too, found by their expressions. The
 * keys' expressions travel in custom_exprs, for EXPLAIN.
 */
static Plan *
sort_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		  List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);
	List	   *data;
	List	   *columns = NIL;
	List	   *keys = NIL;
	List	   *key_exprs = NIL;
	TessPlanWriter *writer;

	tess_path_get_info(best_path, &info);
	if (!tess_plan_child(best_path, custom_plans, 0, &child))
		elog(ERROR, "TessSort expected a batch child");
	/* Over a set operation's rows: its columns are the child's targets. */
	tlist = (List *) tess_plan_setop_columns((Node *) tlist, child.plan);
	info.expressions = (List *) tess_plan_setop_columns((Node *) info.expressions, child.plan);
	data = (List *) info.node_data;
	foreach_node(TargetEntry, entry, tlist)
	{
		TargetEntry *found = tlist_member(entry->expr, child.plan->targetlist);
		int			column = found == NULL ? -1 :
			tess_layout_column(&child.layout, found->resno - 1);

		if (column < 0)
			elog(ERROR, "TessSort target is missing from its child");
		columns = lappend_int(columns, column);
	}
	foreach_ptr(Expr, expr, info.expressions)
	{
		TargetEntry *found = tlist_member(expr, tlist);

		if (found == NULL)
			elog(ERROR, "TessSort key is missing from its targets");
		keys = lappend_int(keys, found->resno - 1);
		key_exprs = lappend(key_exprs, copyObject(expr));
	}
	writer = tess_plan_writer_create(TESS_SORT_DATA, TESS_SORT_DATA_VERSION);
	tess_plan_write_int_list(writer, "columns", columns);
	tess_plan_write_int_list(writer, "keys", keys);
	tess_plan_write_int_list(writer, "kinds", linitial(data));
	tess_plan_write_int_list(writer, "flags", lsecond(data));
	tess_plan_write_int_list(writer, "sortops", lthird(data));
	tess_plan_write_int_list(writer, "collations", lfourth(data));
	config.methods = &tess_sort_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.expressions = key_exprs;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

void
tess_sort_planner_init(void)
{
	previous_create_upper_paths_hook = create_upper_paths_hook;
	create_upper_paths_hook = create_upper_paths;
}

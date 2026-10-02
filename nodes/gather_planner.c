#include "postgres.h"

#include "access/htup_details.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/paramassign.h"
#include "optimizer/pathnode.h"
#include "optimizer/planner.h"
#include "storage/proc.h"
#include "utils/lsyscache.h"

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * The planner of TessGather and TessGatherMerge: the paths that stand in
 * for the core's Gather and Gather Merge over a batch child, the hold on
 * the parallel-aware flag of the nodes below them, and the plans they
 * write for the executor (gather.c). See docs/nodes.md.
 */

static create_upper_paths_hook_type previous_create_upper_paths_hook = NULL;
static planner_hook_type previous_planner_hook = NULL;

/*
 * The parallel-aware plan nodes under a TessGather of the planning under
 * way, their flag held back until it is done: finalize_plan requires one
 * of the core's Gathers above every parallel-aware node once a query has
 * executor parameters, and knows no custom scan that gathers. The flag
 * gets a node its shared-memory callbacks; the rescan parameter a core
 * Gather adds for its nodes the node does without, rescanning its child
 * and reinitializing the shared memory itself.
 */
static List *held_parallel_aware = NIL;

static Plan *gather_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
						 List *tlist, List *clauses, List *custom_plans);
static Plan *gather_merge_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
							   List *tlist, List *clauses, List *custom_plans);
static Plan *send_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
					   List *tlist, List *clauses, List *custom_plans);

static const CustomPathMethods gather_path_methods = {
	.CustomName = "TessGather",
	.PlanCustomPath = gather_plan,
};

static const CustomPathMethods gather_merge_path_methods = {
	.CustomName = "TessGatherMerge",
	.PlanCustomPath = gather_merge_plan,
};

static const CustomPathMethods send_path_methods = {
	.CustomName = "TessSend",
	.PlanCustomPath = send_plan,
};

/* ---------------------------------------------------------------- planning */

/*
 * The batch path a gather of subpath reads: subpath itself, or, under a
 * projection the planner put there for a batch node that projects, a copy
 * of that node taking the projection's target, as the core's plan would
 * give it the projection. NULL where the core's gather stays: no worker,
 * or no batch path the workers may run of a column at least.
 */
static Path *
gathered_batch_path(Path *subpath, int num_workers)
{
	if (IsA(subpath, ProjectionPath) && ((ProjectionPath *) subpath)->dummypp)
	{
		ProjectionPath *projection = (ProjectionPath *) subpath;
		Path	   *below = projection->subpath;
		CustomPath *copy;

		if (tess_path_node(below) == NULL || !IsA(below, CustomPath) ||
			(((CustomPath *) below)->flags & CUSTOMPATH_SUPPORT_PROJECTION) == 0)
			return NULL;
		copy = makeNode(CustomPath);
		*copy = *(CustomPath *) below;
		copy->path.pathtarget = projection->path.pathtarget;
		subpath = &copy->path;
	}
	if (num_workers <= 0 || tess_path_node(subpath) == NULL || !subpath->parallel_safe ||
		subpath->param_info != NULL ||
		list_length(subpath->pathtarget->exprs) == 0 ||
		list_length(subpath->pathtarget->exprs) > MaxTupleAttributeNumber)
		return NULL;
	return subpath;
}

/*
 * TessSend over subpath and the gathering node over it, with the core
 * node's rows, costs and path keys; send_data the keys TessSend's messages
 * carry, if any. A gather that projects, of a target the workers may not
 * compute, becomes a projection over the node, which emits subpath's.
 */
static Path *
make_send_and_gather(PlannerInfo *root, Path *gather, Path *subpath, int num_workers,
					 List *send_data, const CustomPathMethods *methods, const TessNode *node)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	CustomPath *send;
	CustomPath *path;

	config.template_path = subpath;
	config.methods = &send_path_methods;
	config.node = &tess_send_node;
	config.children = list_make1(subpath);
	config.node_data = (Node *) send_data;
	send = tess_path_create(&config);
	/* Parallel-aware for the chunk of shared memory its queues take. */
	send->path.parallel_aware = true;
	config = (TessPathConfig) TESS_STRUCT_INITIALIZER(TessPathConfig);
	config.template_path = gather;
	config.methods = methods;
	config.node = node;
	config.children = list_make1(&send->path);
	config.node_data = (Node *) list_make1(makeInteger(num_workers));
	path = tess_path_create(&config);
	if (equal(gather->pathtarget->exprs, subpath->pathtarget->exprs))
		return &path->path;
	path->path.pathtarget = subpath->pathtarget;
	return (Path *) create_projection_path(root, gather->parent, &path->path, gather->pathtarget);
}

static Path *
make_gather_path(PlannerInfo *root, GatherPath *gather)
{
	Path	   *subpath = gathered_batch_path(gather->subpath, gather->num_workers);

	/* A single copy runs in one worker without the leader: only the core's plans make it. */
	if (gather->single_copy || subpath == NULL)
		return NULL;
	return make_send_and_gather(root, &gather->path, subpath, gather->num_workers, NIL,
								&gather_path_methods, &tess_gather_node);
}

/*
 * The node's path in place of a Gather Merge over a batch path whose path
 * keys the sort kernels order by: TessGatherMerge over TessSend, which
 * sends each row's key words with it. NULL where the core's stays.
 */
static Path *
make_gather_merge_path(PlannerInfo *root, GatherMergePath *gather)
{
	const TessKernelOps *kernels = tess_runtime_kernels();
	Path	   *subpath = gathered_batch_path(gather->subpath, gather->num_workers);
	List	   *places = NIL;
	List	   *kinds = NIL;
	List	   *flags = NIL;
	List	   *sortops = NIL;
	List	   *collations = NIL;
	int			nkeys = list_length(gather->path.pathkeys);

	if (subpath == NULL ||
		nkeys == 0 || nkeys > TESS_TABLE_MAX_KEYS || kernels == NULL)
		return NULL;
	foreach_node(PathKey, pathkey, gather->path.pathkeys)
	{
		TessSortKey key;
		int			place;
		Oid			sortop = InvalidOid;
		Oid			collation = InvalidOid;

		/* As TessSort takes them: a key of another type by its comparison. */
		if (tess_sort_key_of(pathkey, subpath->pathtarget, subpath->parent->relids,
							 &place, &key))
		{
			Oid			type = exprType(list_nth(subpath->pathtarget->exprs, place));

			sortop = get_opfamily_member_for_cmptype(pathkey->pk_opfamily, type, type,
													 pathkey->pk_cmptype);
			collation = pathkey->pk_eclass->ec_collation;
		}
		else if (tess_sort_generic_key(pathkey, subpath->pathtarget,
									   subpath->parent->relids, &place, &sortop, &collation))
		{
			key.kind = TESS_SORT_KIND_GENERIC;
			key.flags = (pathkey->pk_cmptype == COMPARE_GT ? TESS_SORT_DESCENDING : 0) |
				(pathkey->pk_nulls_first ? TESS_SORT_NULLS_FIRST : 0);
		}
		else
			return NULL;
		places = lappend_int(places, place);
		kinds = lappend_int(kinds, (int) key.kind);
		flags = lappend_int(flags, (int) key.flags);
		sortops = lappend_int(sortops, (int) sortop);
		collations = lappend_int(collations, (int) collation);
	}
	return make_send_and_gather(root, &gather->path, subpath, gather->num_workers,
								list_make5(places, kinds, flags, sortops, collations),
								&gather_merge_path_methods, &tess_gather_merge_node);
}

/*
 * A gather's cost at the node's share of parallel_tuple_cost for rows rows,
 * factor times each as the core's costs them.
 */
static void
discount_transfer(Path *path, double rows, double factor)
{
	path->total_cost -= (1.0 - tess_gather_tuple_share) * factor * parallel_tuple_cost * rows;
}

/*
 * TessGather over the cheapest partial path of rel, where that is a batch
 * path, at the node's cost of a row: the core gathers the same partial
 * path at its own cost after the hooks that call this, and add_path keeps
 * the cheaper. A base or join relation only, as the core's gathers; for
 * the topmost one the core applies the final target to it as to any path.
 *
 * The gather reads a copy of the partial path. The join hook runs once a
 * pair of inputs, and a later pair may add a partial path that dominates
 * this one: add_partial_path frees the one it drops, which the gather
 * would still read, and the memory goes to a path made later, a
 * projection over the gather itself among them.
 */
void
tess_gather_add_paths(PlannerInfo *root, RelOptInfo *rel)
{
	Path	   *subpath;
	CustomPath *copy;
	GatherPath *gather;
	Path	   *path;
	double		rows;

	if (!*tess_runtime_api()->settings->enable || !tess_batch_gather ||
		!rel->consider_parallel || rel->partial_pathlist == NIL ||
		(rel->reloptkind != RELOPT_BASEREL && rel->reloptkind != RELOPT_JOINREL))
		return;
	subpath = linitial(rel->partial_pathlist);
	if (tess_path_node(subpath) == NULL)
		return;
	/* A batch path is a CustomPath; its children outlive it. */
	copy = makeNode(CustomPath);
	*copy = *castNode(CustomPath, subpath);
	subpath = &copy->path;
	rows = compute_gather_rows(subpath);
	gather = create_gather_path(root, rel, subpath, rel->reltarget, NULL, &rows);
	path = make_gather_path(root, gather);
	if (path == NULL)
		return;
	discount_transfer(path, gather->path.rows, 1.0);
	add_path(rel, path);
}

/*
 * TessGather over subpath, a partial batch path of rel, at the node's cost
 * of a row, with subpath's target: for a batch parent above, which the
 * core's gather would give rows. NULL where the node cannot gather it.
 */
Path *
tess_gather_path(PlannerInfo *root, RelOptInfo *rel, Path *subpath)
{
	GatherPath *gather;
	Path	   *path;
	double		rows;

	if (!*tess_runtime_api()->settings->enable || !tess_batch_gather)
		return NULL;
	rows = compute_gather_rows(subpath);
	gather = create_gather_path(root, rel, subpath, subpath->pathtarget, NULL, &rows);
	path = make_gather_path(root, gather);
	if (path == NULL)
		return NULL;
	discount_transfer(path, gather->path.rows, 1.0);
	return path;
}

/*
 * TessGatherMerge over sorted, a partial path of the node's sort in the
 * ordered relation, at the node's cost of a row, projected to target when
 * that differs; NULL where the node cannot merge it.
 */
Path *
tess_gather_merge_path(PlannerInfo *root, RelOptInfo *rel, Path *sorted, PathTarget *target)
{
	GatherMergePath *gather;
	Path	   *path;
	double		rows;

	if (!*tess_runtime_api()->settings->enable || !tess_batch_gather)
		return NULL;
	rows = compute_gather_rows(sorted);
	gather = create_gather_merge_path(root, rel, sorted, sorted->pathtarget, sorted->pathkeys,
									  NULL, &rows);
	path = make_gather_merge_path(root, gather);
	if (path == NULL)
		return NULL;
	discount_transfer(path, gather->path.rows, 1.05);
	if (!equal(path->pathtarget->exprs, target->exprs))
		path = (Path *) create_projection_path(root, rel, path, target);
	return path;
}

/*
 * Replace, under path, every Gather over a batch path by the node's path,
 * through the kinds of path that hold others; a kind not known here is
 * left as it is, and so is what is below it.
 */
static Path *
replace_gathers(PlannerInfo *root, Path *path)
{
	if (path == NULL)
		return NULL;
	switch (nodeTag(path))
	{
		case T_GatherPath:
			{
				Path	   *replaced = make_gather_path(root, (GatherPath *) path);

				return replaced != NULL ? replaced : path;
			}
		case T_GatherMergePath:
			{
				Path	   *replaced = make_gather_merge_path(root, (GatherMergePath *) path);

				return replaced != NULL ? replaced : path;
			}
		case T_ProjectionPath:
			((ProjectionPath *) path)->subpath =
				replace_gathers(root, ((ProjectionPath *) path)->subpath);
			break;
		case T_ProjectSetPath:
			((ProjectSetPath *) path)->subpath =
				replace_gathers(root, ((ProjectSetPath *) path)->subpath);
			break;
		case T_SortPath:
			((SortPath *) path)->subpath =
				replace_gathers(root, ((SortPath *) path)->subpath);
			break;
		case T_IncrementalSortPath:
			((IncrementalSortPath *) path)->spath.subpath =
				replace_gathers(root, ((IncrementalSortPath *) path)->spath.subpath);
			break;
		case T_AggPath:
			((AggPath *) path)->subpath =
				replace_gathers(root, ((AggPath *) path)->subpath);
			break;
		case T_GroupPath:
			((GroupPath *) path)->subpath =
				replace_gathers(root, ((GroupPath *) path)->subpath);
			break;
		case T_UniquePath:
			((UniquePath *) path)->subpath =
				replace_gathers(root, ((UniquePath *) path)->subpath);
			break;
		case T_WindowAggPath:
			((WindowAggPath *) path)->subpath =
				replace_gathers(root, ((WindowAggPath *) path)->subpath);
			break;
		case T_LimitPath:
			((LimitPath *) path)->subpath =
				replace_gathers(root, ((LimitPath *) path)->subpath);
			break;
		case T_LockRowsPath:
			((LockRowsPath *) path)->subpath =
				replace_gathers(root, ((LockRowsPath *) path)->subpath);
			break;
		case T_MaterialPath:
			((MaterialPath *) path)->subpath =
				replace_gathers(root, ((MaterialPath *) path)->subpath);
			break;
		case T_MemoizePath:
			((MemoizePath *) path)->subpath =
				replace_gathers(root, ((MemoizePath *) path)->subpath);
			break;
		case T_NestPath:
		case T_MergePath:
		case T_HashPath:
			{
				JoinPath   *join = (JoinPath *) path;

				join->outerjoinpath = replace_gathers(root, join->outerjoinpath);
				join->innerjoinpath = replace_gathers(root, join->innerjoinpath);
				break;
			}
		case T_GroupingSetsPath:
			((GroupingSetsPath *) path)->subpath =
				replace_gathers(root, ((GroupingSetsPath *) path)->subpath);
			break;
		case T_ModifyTablePath:
			((ModifyTablePath *) path)->subpath =
				replace_gathers(root, ((ModifyTablePath *) path)->subpath);
			break;
		case T_SetOpPath:
			((SetOpPath *) path)->leftpath = replace_gathers(root, ((SetOpPath *) path)->leftpath);
			((SetOpPath *) path)->rightpath = replace_gathers(root, ((SetOpPath *) path)->rightpath);
			break;
		case T_RecursiveUnionPath:
			((RecursiveUnionPath *) path)->leftpath =
				replace_gathers(root, ((RecursiveUnionPath *) path)->leftpath);
			((RecursiveUnionPath *) path)->rightpath =
				replace_gathers(root, ((RecursiveUnionPath *) path)->rightpath);
			break;
		case T_MinMaxAggPath:
			/* Each aggregate's subquery is planned without the final stage's hook. */
			foreach_node(MinMaxAggInfo, info, ((MinMaxAggPath *) path)->mmaggregates)
				info->path = replace_gathers(info->subroot, info->path);
			break;
		case T_AppendPath:
			{
				ListCell   *lc;

				foreach(lc, ((AppendPath *) path)->subpaths)
					lfirst(lc) = replace_gathers(root, lfirst(lc));
				break;
			}
		case T_MergeAppendPath:
			{
				ListCell   *lc;

				foreach(lc, ((MergeAppendPath *) path)->subpaths)
					lfirst(lc) = replace_gathers(root, lfirst(lc));
				break;
			}
		case T_CustomPath:
			{
				CustomPath *custom = (CustomPath *) path;
				const TessNode *node = tess_path_node(path);
				ListCell   *lc;

				foreach(lc, custom->custom_paths)
					lfirst(lc) = replace_gathers(root, lfirst(lc));
				/*
				 * A pack over a Gather made batches of its rows: over the
				 * node's path, which gives batches, it goes.
				 */
				if (node != NULL && strcmp(node->name, TESS_PACK_NODE_NAME) == 0 &&
					list_length(custom->custom_paths) == 1 &&
					(tess_path_node(linitial(custom->custom_paths)) == &tess_gather_node ||
					 tess_path_node(linitial(custom->custom_paths)) == &tess_gather_merge_node) &&
					equal(path->pathtarget->exprs,
						  ((Path *) linitial(custom->custom_paths))->pathtarget->exprs))
					return linitial(custom->custom_paths);
				break;
			}
		default:
			break;
	}
	return path;
}

/*
 * Once the query's paths are final, every Gather over a batch path in
 * them gives way to the node's: the choice between plans stays the
 * core's, made at its costs.
 */
static void
create_upper_paths(PlannerInfo *root, UpperRelationKind stage,
				   RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra)
{
	ListCell   *lc;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel, extra);
	if (!*tess_runtime_api()->settings->enable || !tess_batch_gather ||
		stage != UPPERREL_FINAL)
		return;
	foreach(lc, output_rel->pathlist)
		lfirst(lc) = replace_gathers(root, lfirst(lc));
}

/* TessSend keeps its child's layout: it sends the child's targets in order. */
static Plan *
send_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path, List *tlist,
		  List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	List	   *data = NIL;
	TessPlanWriter *writer;

	tess_path_get_info(best_path, &info);
	data = (List *) info.node_data;
	/* The keys a merge above orders by, none under TessGather. */
	writer = tess_plan_writer_create(TESS_SEND_DATA, TESS_SEND_DATA_VERSION);
	tess_plan_write_int_list(writer, "keys", data == NIL ? NIL : linitial(data));
	tess_plan_write_int_list(writer, "kinds", data == NIL ? NIL : lsecond(data));
	tess_plan_write_int_list(writer, "flags", data == NIL ? NIL : lthird(data));
	tess_plan_write_int_list(writer, "sortops", data == NIL ? NIL : lfourth(data));
	tess_plan_write_int_list(writer, "collations", data == NIL ? NIL : list_nth(data, 4));
	config.methods = &tess_send_scan_methods;
	config.layout_policy = TESS_LAYOUT_PRESERVE_CHILD;
	config.layout_child = 0;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

/* Hold back the parallel-aware flag of plan and the nodes below it. */
static void
hold_parallel_aware(Plan *plan)
{
	ListCell   *lc;

	if (plan == NULL)
		return;
	check_stack_depth();
	if (plan->parallel_aware)
	{
		plan->parallel_aware = false;
		held_parallel_aware = lappend(held_parallel_aware, plan);
	}
	hold_parallel_aware(plan->lefttree);
	hold_parallel_aware(plan->righttree);
	switch (nodeTag(plan))
	{
		case T_CustomScan:
			foreach(lc, ((CustomScan *) plan)->custom_plans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_Append:
			foreach(lc, ((Append *) plan)->appendplans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_MergeAppend:
			foreach(lc, ((MergeAppend *) plan)->mergeplans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_BitmapAnd:
			foreach(lc, ((BitmapAnd *) plan)->bitmapplans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_BitmapOr:
			foreach(lc, ((BitmapOr *) plan)->bitmapplans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_SubqueryScan:
			hold_parallel_aware(((SubqueryScan *) plan)->subplan);
			break;
		default:
			break;
	}
}

/*
 * Whether plan or a node below it is parallel-aware; if so, plan takes
 * param among its external and all parameters, as finalize_plan gives the
 * nodes between a Gather and the parallel-aware nodes below it the
 * Gather's rescan_param, so that a rescan of the gather reaches them.
 */
static bool
take_rescan_param(Plan *plan, int param)
{
	bool		aware;
	ListCell   *lc;

	if (plan == NULL)
		return false;
	check_stack_depth();
	aware = plan->parallel_aware;
	aware |= take_rescan_param(plan->lefttree, param);
	aware |= take_rescan_param(plan->righttree, param);
	switch (nodeTag(plan))
	{
		case T_CustomScan:
			foreach(lc, ((CustomScan *) plan)->custom_plans)
				aware |= take_rescan_param(lfirst(lc), param);
			break;
		case T_Append:
			foreach(lc, ((Append *) plan)->appendplans)
				aware |= take_rescan_param(lfirst(lc), param);
			break;
		case T_MergeAppend:
			foreach(lc, ((MergeAppend *) plan)->mergeplans)
				aware |= take_rescan_param(lfirst(lc), param);
			break;
		case T_BitmapAnd:
			foreach(lc, ((BitmapAnd *) plan)->bitmapplans)
				aware |= take_rescan_param(lfirst(lc), param);
			break;
		case T_BitmapOr:
			foreach(lc, ((BitmapOr *) plan)->bitmapplans)
				aware |= take_rescan_param(lfirst(lc), param);
			break;
		case T_SubqueryScan:
			aware |= take_rescan_param(((SubqueryScan *) plan)->subplan, param);
			break;
		default:
			break;
	}
	if (aware)
	{
		plan->extParam = bms_add_member(plan->extParam, param);
		plan->allParam = bms_add_member(plan->allParam, param);
	}
	return aware;
}

/*
 * Every TessGather and TessGatherMerge at or below plan gives its rescan
 * parameter to the nodes below it, as finalize_plan would have, had it
 * known the node for a gather: it saw no parallel-aware node under it
 * (hold_parallel_aware).
 */
static void
give_rescan_params(Plan *plan)
{
	ListCell   *lc;

	if (plan == NULL)
		return;
	check_stack_depth();
	if (IsA(plan, CustomScan) &&
		(((CustomScan *) plan)->methods == &tess_gather_scan_methods ||
		 ((CustomScan *) plan)->methods == &tess_gather_merge_scan_methods))
	{
		CustomScan *cscan = (CustomScan *) plan;
		TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
		TessPlanReader *reader;
		int			param;

		tess_plan_get_info(cscan, &info);
		reader = tess_plan_reader_create((List *) info.node_data, TESS_GATHER_DATA,
										 TESS_GATHER_DATA_VERSION);
		(void) tess_plan_read_int(reader, "workers");
		param = tess_plan_read_int(reader, "rescan_param");
		tess_plan_reader_finish(reader);
		take_rescan_param(linitial(cscan->custom_plans), param);
		return;
	}
	give_rescan_params(plan->lefttree);
	give_rescan_params(plan->righttree);
	switch (nodeTag(plan))
	{
		case T_CustomScan:
			foreach(lc, ((CustomScan *) plan)->custom_plans)
				give_rescan_params(lfirst(lc));
			break;
		case T_Append:
			foreach(lc, ((Append *) plan)->appendplans)
				give_rescan_params(lfirst(lc));
			break;
		case T_MergeAppend:
			foreach(lc, ((MergeAppend *) plan)->mergeplans)
				give_rescan_params(lfirst(lc));
			break;
		case T_SubqueryScan:
			give_rescan_params(((SubqueryScan *) plan)->subplan);
			break;
		default:
			break;
	}
}

/* The planning done, the flags held back come back; a nested planning keeps its own. */
static PlannedStmt *
gather_planner(Query *parse, const char *query_string, int cursorOptions,
			   ParamListInfo boundParams, ExplainState *es)
{
	List	   *outer = held_parallel_aware;
	PlannedStmt *result;

	held_parallel_aware = NIL;
	PG_TRY();
	{
		result = previous_planner_hook != NULL ?
			previous_planner_hook(parse, query_string, cursorOptions, boundParams, es) :
			standard_planner(parse, query_string, cursorOptions, boundParams, es);
	}
	PG_CATCH();
	{
		held_parallel_aware = outer;
		PG_RE_THROW();
	}
	PG_END_TRY();
	foreach_ptr(Plan, plan, held_parallel_aware)
		plan->parallel_aware = true;
	list_free(held_parallel_aware);
	held_parallel_aware = outer;
	/* The statement's plan and its subplans: initplans and subplans alike. */
	give_rescan_params(result->planTree);
	foreach_ptr(Plan, subplan, result->subplans)
		give_rescan_params(subplan);
	return result;
}

/* TessGather and TessGatherMerge: a column per target, the targets TessSend sends. */
static Plan *
gather_plan_of(PlannerInfo *root, CustomPath *best_path, List *tlist, List *custom_plans,
			   const CustomScanMethods *methods)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanWriter *writer;

	tess_path_get_info(best_path, &info);
	/* The plan launches workers, as the core's Gather's does. */
	root->glob->parallelModeNeeded = true;
	writer = tess_plan_writer_create(TESS_GATHER_DATA, TESS_GATHER_DATA_VERSION);
	tess_plan_write_int(writer, "workers", intVal(linitial((List *) info.node_data)));
	/* The parameter that tells the leader's own part to rescan, a Gather's rescan_param. */
	tess_plan_write_int(writer, "rescan_param", assign_special_exec_param(root));
	config.methods = methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	foreach_ptr(Plan, child, custom_plans)
		hold_parallel_aware(child);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

static Plan *
gather_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path, List *tlist,
			List *clauses, List *custom_plans)
{
	return gather_plan_of(root, best_path, tlist, custom_plans, &tess_gather_scan_methods);
}

static Plan *
gather_merge_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path, List *tlist,
				  List *clauses, List *custom_plans)
{
	return gather_plan_of(root, best_path, tlist, custom_plans, &tess_gather_merge_scan_methods);
}

void
tess_gather_planner_init(void)
{
	previous_create_upper_paths_hook = create_upper_paths_hook;
	create_upper_paths_hook = create_upper_paths;
	previous_planner_hook = planner_hook;
	planner_hook = gather_planner;
}

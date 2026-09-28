#include "postgres.h"

#include "catalog/pg_class.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "optimizer/restrictinfo.h"
#include "optimizer/tlist.h"
#include "parser/parsetree.h"

#include "tessera/expr.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * The planner side of TessFilter: a path over a base relation whose
 * clauses the node takes away from the sequential scan below the pack
 * node, so that the scan produces every row and the node filters them:
 * the leading clauses the expression compiler supports by batches, the
 * rest row by row, in the planner's order. See docs/nodes.md.
 */

/* The path costs a fraction of the scan's: there is no cost model yet. */
#define FILTER_COST_FACTOR 0.9

static set_rel_pathlist_hook_type previous_set_rel_pathlist_hook = NULL;

static Plan *filter_plan(PlannerInfo *root, RelOptInfo *rel,
						 CustomPath *best_path, List *tlist, List *clauses,
						 List *custom_plans);

static const CustomPathMethods filter_path_methods = {
	.CustomName = "TessFilter",
	.PlanCustomPath = filter_plan,
};

const CustomScanMethods tess_filter_scan_methods = {
	.CustomName = "TessFilter",
	.CreateCustomScanState = tess_filter_create_state,
};

/*
 * A plain scan of one heap table in a SELECT, with nothing parameterized:
 * a base relation, or a partition or inheritance child, whose clauses the
 * core translated from its parent's.
 */
static bool
relation_supported(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	return IS_SIMPLE_REL(rel) &&
		rte->rtekind == RTE_RELATION && rte->relkind == RELKIND_RELATION &&
		!rte->inh && rte->tablesample == NULL &&
		root->parse->commandType == CMD_SELECT &&
		root->parse->rowMarks == NIL && rel->lateral_relids == NULL;
}

/*
 * The clauses in the order the planner evaluates them, as its static
 * order_qual_clauses sorts a plan's quals: by cost within security
 * levels, a cheap leakproof clause counting as level zero, equals in
 * their order; an insertion sort, which keeps them so.
 */
List *
tess_order_clauses(PlannerInfo *root, List *rinfos)
{
	int			count = list_length(rinfos);
	RestrictInfo **items;
	Cost	   *costs;
	Index	   *levels;
	List	   *ordered = NIL;
	int			index = 0;

	if (count <= 1)
		return list_copy(rinfos);
	items = palloc_array(RestrictInfo *, count);
	costs = palloc_array(Cost, count);
	levels = palloc_array(Index, count);
	foreach_node(RestrictInfo, rinfo, rinfos)
	{
		QualCost	cost;
		Cost		item_cost;
		Index		item_level;
		int			at = index;

		cost_qual_eval_node(&cost, (Node *) rinfo, root);
		item_cost = cost.per_tuple;
		item_level = rinfo->leakproof && item_cost < 10 * cpu_operator_cost ?
			0 : rinfo->security_level;
		while (at > 0 && (levels[at - 1] > item_level ||
						  (levels[at - 1] == item_level && costs[at - 1] > item_cost)))
		{
			items[at] = items[at - 1];
			costs[at] = costs[at - 1];
			levels[at] = levels[at - 1];
			at--;
		}
		items[at] = rinfo;
		costs[at] = item_cost;
		levels[at] = item_level;
		index++;
	}
	for (index = 0; index < count; index++)
		ordered = lappend(ordered, items[index]);
	return ordered;
}

/*
 * The clause the planner evaluates first. NULL with a pseudoconstant
 * clause: it makes the planner wrap each scan of the relation in a gating
 * Result, which the plan would then find in place of its children.
 */
static RestrictInfo *
first_clause(PlannerInfo *root, RelOptInfo *rel)
{
	foreach_ptr(RestrictInfo, rinfo, rel->baserestrictinfo)
	{
		if (rinfo->pseudoconstant)
			return NULL;
	}
	if (rel->baserestrictinfo == NIL)
		return NULL;
	return linitial(tess_order_clauses(root, rel->baserestrictinfo));
}

/* Whether the node has batch work: the first clause is a batch filter. */
static bool
clauses_supported(PlannerInfo *root, RelOptInfo *rel)
{
	RestrictInfo *first = first_clause(root, rel);

	return first != NULL &&
		tess_expr_supports_filter((Node *) first->clause, rel->relid);
}

/* The relation's targets and the clauses' columns, which the filter reads. */
static PathTarget *
filter_input_target(PlannerInfo *root, RelOptInfo *rel)
{
	PathTarget *target = copy_pathtarget(rel->reltarget);
	List	   *clauses = extract_actual_clauses(rel->baserestrictinfo, false);

	add_new_columns_to_pathtarget(target,
								  pull_var_clause((Node *) clauses,
												  PVC_RECURSE_PLACEHOLDERS));
	return set_pathtarget_cost_width(root, target);
}

/*
 * The batch child over a copy of the scan: add_path frees the core path
 * the node's path dominates. The scan reads the clauses' columns as well
 * as the relation's targets, since the node evaluates the clauses. A node
 * that reads the relation in batches natively evaluates no clause and
 * comes first; otherwise the pack node stands over the core scan.
 */
static Path *
make_child_path(PlannerInfo *root, RelOptInfo *rel, const Path *seqscan)
{
	Path	   *copy = makeNode(Path);
	Path	   *child;

	*copy = *seqscan;
	copy->pathtarget = filter_input_target(root, rel);
	child = tess_batch_scan_path(root, copy);
	return child != NULL ? child : tess_batch_input_path(root, copy);
}

/* The node's path over the child, with the scan's properties and rows. */
static CustomPath *
make_filter_path(RelOptInfo *rel, const Path *seqscan, Path *child)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path		template = *seqscan;

	template.total_cost *= FILTER_COST_FACTOR;
	config.template_path = &template;
	config.methods = &filter_path_methods;
	config.node = &tess_filter_node;
	config.children = list_make1(child);
	/* Expressions in the targets are computed over the batches. */
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	return tess_path_create(&config);
}

/* The unparameterized sequential scan of the list, or NULL. */
static Path *
find_seqscan(const List *pathlist)
{
	foreach_ptr(Path, path, pathlist)
	{
		if (path->pathtype == T_SeqScan && path->param_info == NULL)
			return path;
	}
	return NULL;
}

/*
 * TessFilter over a base relation whose clauses all run row by row, in
 * place of its sequential scan (a partial one gives a partial path), or
 * NULL. The relation gets it as a path (add_row_filter_paths), and an
 * inner or semi hash join takes it for its outer side: the join's Bloom
 * filter then removes rows before the row-wise clauses run.
 */
Path *
tess_filter_row_path(PlannerInfo *root, RelOptInfo *rel, Path *seqscan)
{
	Path	   *child;

	if (!*tess_runtime_api()->settings->enable || seqscan == NULL ||
		seqscan->pathtype != T_SeqScan || seqscan->param_info != NULL ||
		!relation_supported(root, rel, planner_rt_fetch(rel->relid, root)) ||
		first_clause(root, rel) == NULL || clauses_supported(root, rel) ||
		(seqscan->parallel_workers > 0 &&
		 (!seqscan->parallel_aware || !rel->consider_parallel)))
		return NULL;
	child = make_child_path(root, rel, seqscan);
	return child != NULL ? (Path *) make_filter_path(rel, seqscan, child) : NULL;
}

/*
 * The node's path in place of the sequential scan, and a partial one in
 * place of the parallel sequential scan, so that a Gather above runs the
 * node in every participant over that participant's share of the pages;
 * the partial path keeps the core scan's number of workers and rows per
 * participant, and is parallel-aware for the counters the node shares.
 */
static void
add_filter_paths(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	Path	   *seqscan;
	Path	   *partial;
	Path	   *child;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || !clauses_supported(root, rel))
		return;
	seqscan = find_seqscan(rel->pathlist);
	if (seqscan == NULL)
		return;
	child = make_child_path(root, rel, seqscan);
	if (child == NULL)
		return;
	add_path(rel, (Path *) make_filter_path(rel, seqscan, child));
	partial = find_seqscan(rel->partial_pathlist);
	if (partial == NULL || !partial->parallel_aware || !rel->consider_parallel)
		return;
	child = make_child_path(root, rel, partial);
	/*
	 * Parallel-aware as the template is: the child divides the work, and
	 * the node shares its counters, which takes the callbacks.
	 */
	if (child != NULL)
		add_partial_path(rel, (Path *) make_filter_path(rel, partial, child));
}

/*
 * The native scan in place of the sequential scan of a relation without
 * clauses, and in place of the parallel one: it is faster than the core's
 * under any parent, a row-wise one included, since it pins a page once
 * and deforms only the columns read (bench/pg/rowwise). The path costs
 * the filter's fraction of the scan's: there is no cost model yet.
 */
static void
add_scan_paths(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	Path	   *seqscan;
	Path	   *copy;
	Path	   *scan;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || rel->baserestrictinfo != NIL)
		return;
	seqscan = find_seqscan(rel->pathlist);
	if (seqscan == NULL)
		return;
	/* add_path frees a core path the node's dominates: the node keeps a copy. */
	copy = makeNode(Path);
	*copy = *seqscan;
	scan = tess_batch_scan_path(root, copy);
	if (scan == NULL)
		return;
	scan->total_cost *= FILTER_COST_FACTOR;
	add_path(rel, scan);
	seqscan = find_seqscan(rel->partial_pathlist);
	if (seqscan == NULL || !seqscan->parallel_aware || !rel->consider_parallel)
		return;
	copy = makeNode(Path);
	*copy = *seqscan;
	scan = tess_batch_scan_path(root, copy);
	if (scan == NULL)
		return;
	scan->total_cost *= FILTER_COST_FACTOR;
	add_partial_path(rel, scan);
}

/*
 * TessFilter over a relation whose clauses all run row by row, in place
 * of its sequential scan and its parallel one: the node's rows cost less
 * than the core's scan under any parent (bench/pg/rowwise), and a batch
 * parent above reads its batches instead of a pack's copies.
 */
static void
add_row_filter_paths(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	Path	   *path;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || rel->baserestrictinfo == NIL)
		return;
	path = tess_filter_row_path(root, rel, find_seqscan(rel->pathlist));
	if (path == NULL)
		return;
	add_path(rel, path);
	path = tess_filter_row_path(root, rel, find_seqscan(rel->partial_pathlist));
	if (path != NULL)
		add_partial_path(rel, path);
}

/*
 * TessFilter over the node's scan of a bitmap's pages in place of each of
 * the core's bitmap heap scans of the relation, unparameterized and
 * serial: the filter evaluates every clause, the index's among them,
 * which a lossy page needs rechecked and an exact one does not (a filter
 * in batches is cheap), and the path costs the filter's fraction of the
 * core's.
 */
static void
add_bitmap_paths(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	List	   *bitmaps = NIL;

	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || first_clause(root, rel) == NULL)
		return;
	/* add_path frees a core path the node's dominates: copies are taken first. */
	foreach_ptr(Path, path, rel->pathlist)
	{
		BitmapHeapPath *copy;

		if (!IsA(path, BitmapHeapPath) || path->param_info != NULL ||
			path->parallel_aware)
			continue;
		copy = palloc_object(BitmapHeapPath);
		memcpy(copy, path, sizeof(BitmapHeapPath));
		bitmaps = lappend(bitmaps, copy);
	}
	foreach_ptr(BitmapHeapPath, bitmap, bitmaps)
	{
		Path	   *scan = tess_heap_bitmap_path(root, bitmap,
												 filter_input_target(root, rel));

		if (scan != NULL)
			add_path(rel, (Path *) make_filter_path(rel, &bitmap->path, scan));
	}
}

/* The node's paths, then TessGather over the cheapest partial path, before the core gathers it. */
static void
set_rel_pathlist(PlannerInfo *root, RelOptInfo *rel, Index rti,
				 RangeTblEntry *rte)
{
	if (previous_set_rel_pathlist_hook != NULL)
		previous_set_rel_pathlist_hook(root, rel, rti, rte);
	add_filter_paths(root, rel, rte);
	add_row_filter_paths(root, rel, rte);
	add_scan_paths(root, rel, rte);
	add_bitmap_paths(root, rel, rte);
	tess_gather_add_paths(root, rel);
}

/*
 * The scan below was planned with the relation's clauses, as every scan
 * of the relation is; the node takes them over, so that the scan produces
 * every row. Anything else in that place is a planner change unknown here.
 */
static void
take_clauses(CustomScan *pack, List *clauses)
{
	Plan	   *scan;

	if (list_length(pack->custom_plans) != 1)
		elog(ERROR, "TessFilter expected a pack node with one child");
	scan = linitial(pack->custom_plans);
	if (!IsA(scan, SeqScan) || !equal(scan->qual, clauses))
		elog(ERROR, "TessFilter found its clauses missing from the scan below");
	scan->qual = NIL;
}

/*
 * The scan tuple is the child's target list: entry k is the child's
 * column of its target k. The node's own targets, PostgreSQL's projection
 * among them, are derived from it when the plan is read.
 */
static void
map_scan_tuple(TessLayout *layout, const TessPlanChild *child)
{
	int			ntargets = list_length(child->plan->targetlist);
	int		   *map = ntargets > 0 ? palloc_array(int, ntargets) : NULL;

	layout->ncolumns = child->layout.ncolumns;
	layout->ntargets = ntargets;
	for (int target = 0; target < ntargets; target++)
		map[target] = tess_layout_column(&child->layout, target);
	layout->target_columns = map;
}

static Plan *
filter_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
			List *tlist, List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	List	   *actual = extract_actual_clauses(clauses, false);
	List	   *batch_clauses = NIL;
	List	   *residual = NIL;
	List	   *order = NIL;
	TessPlanWriter *writer;

	if (!tess_plan_child(best_path, custom_plans, 0, &child) ||
		!IsA(child.plan, CustomScan))
		elog(ERROR, "TessFilter expected a batch child");
	/* A native scan was planned without the clauses; a core scan had them. */
	if (strcmp(child.node->name, TESS_HEAP_SCAN_NODE_NAME) == 0)
	{
		if (child.plan->qual != NIL)
			elog(ERROR, "TessFilter expected a scan without clauses");
	}
	else
		take_clauses((CustomScan *) child.plan, actual);
	/*
	 * The clauses arrive in evaluation order and keep it: each runs in
	 * batches if the compiler takes it, else by rows, and the plan data
	 * records which in turn.
	 */
	foreach_ptr(Node, clause, actual)
	{
		bool		batch = tess_expr_supports_filter(clause, rel->relid);

		if (batch)
			batch_clauses = lappend(batch_clauses, clause);
		else
			residual = lappend(residual, clause);
		order = lappend_int(order, batch ? 1 : 0);
	}
	if (order == NIL)
		elog(ERROR, "TessFilter found no clause");
	writer = tess_plan_writer_create(TESS_FILTER_DATA, TESS_FILTER_DATA_VERSION);
	tess_plan_write_int_list(writer, "order", order);
	map_scan_tuple(&layout, &child);
	config.methods = &tess_filter_scan_methods;
	config.layout_policy = TESS_LAYOUT_PROJECTED;
	config.explicit_layout = &layout;
	config.qual = residual;
	config.expressions = batch_clauses;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	config.scan_targetlist = child.plan->targetlist;
	config.scanrelid = rel->relid;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

void
tess_filter_planner_init(void)
{
	previous_set_rel_pathlist_hook = set_rel_pathlist_hook;
	set_rel_pathlist_hook = set_rel_pathlist;
}

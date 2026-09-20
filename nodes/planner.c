#include "postgres.h"

#include "catalog/pg_class.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "optimizer/restrictinfo.h"
#include "optimizer/tlist.h"

#include "tessera/expr.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * The planner side of TessFilter: a path over a base relation whose
 * clauses the node takes away from the sequential scan below the pack
 * node, so that the scan produces every row and the node filters them by
 * batches. See docs/nodes.md.
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

/* A plain scan of one heap table in a SELECT, with nothing parameterized. */
static bool
relation_supported(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	return rel->reloptkind == RELOPT_BASEREL &&
		rte->rtekind == RTE_RELATION && rte->relkind == RELKIND_RELATION &&
		!rte->inh && rte->tablesample == NULL &&
		root->parse->commandType == CMD_SELECT &&
		root->parse->rowMarks == NIL && rel->lateral_relids == NULL;
}

/*
 * Whether the node can take every clause of the relation. A pseudoconstant
 * clause makes the planner wrap each scan of the relation in a gating
 * Result, which the plan would then find in place of its children.
 */
static bool
clauses_supported(RelOptInfo *rel)
{
	if (rel->baserestrictinfo == NIL)
		return false;
	foreach_ptr(RestrictInfo, rinfo, rel->baserestrictinfo)
	{
		if (rinfo->pseudoconstant ||
			!tess_expr_supports_filter((Node *) rinfo->clause, rel->relid))
			return false;
	}
	return true;
}

/*
 * The batch input over a copy of the scan: add_path frees the core path
 * the node's path dominates. The scan reads the clauses' columns as well
 * as the relation's targets, since the node evaluates the clauses.
 */
static Path *
make_child_path(PlannerInfo *root, RelOptInfo *rel, const Path *seqscan)
{
	Path	   *copy = makeNode(Path);
	PathTarget *target = copy_pathtarget(rel->reltarget);
	List	   *clauses = extract_actual_clauses(rel->baserestrictinfo, false);

	*copy = *seqscan;
	add_new_columns_to_pathtarget(target,
								  pull_var_clause((Node *) clauses,
												  PVC_RECURSE_PLACEHOLDERS));
	copy->pathtarget = set_pathtarget_cost_width(root, target);
	return tess_batch_input_path(root, copy);
}

static void
set_rel_pathlist(PlannerInfo *root, RelOptInfo *rel, Index rti,
				 RangeTblEntry *rte)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *seqscan = NULL;
	Path	   *child;
	Path		template;

	if (previous_set_rel_pathlist_hook != NULL)
		previous_set_rel_pathlist_hook(root, rel, rti, rte);
	if (!*tess_runtime_api()->settings->enable ||
		!relation_supported(root, rel, rte) || !clauses_supported(rel))
		return;
	foreach_ptr(Path, path, rel->pathlist)
	{
		if (path->pathtype == T_SeqScan && path->param_info == NULL)
		{
			seqscan = path;
			break;
		}
	}
	if (seqscan == NULL)
		return;
	child = make_child_path(root, rel, seqscan);
	if (child == NULL)
		return;
	template = *seqscan;
	template.total_cost *= FILTER_COST_FACTOR;
	config.template_path = &template;
	config.methods = &filter_path_methods;
	config.node = &tess_filter_node;
	config.children = list_make1(child);
	add_path(rel, (Path *) tess_path_create(&config));
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

/* The node's targets among the child's columns; the rest stay hidden. */
static void
map_targets(TessLayout *layout, List *tlist, const TessPlanChild *child)
{
	int		   *map = NULL;
	int			target = 0;

	layout->ncolumns = child->layout.ncolumns;
	layout->ntargets = list_length(tlist);
	if (layout->ntargets > 0)
		map = palloc_array(int, layout->ntargets);
	foreach_ptr(TargetEntry, entry, tlist)
	{
		TargetEntry *found = tlist_member(entry->expr, child->plan->targetlist);

		if (found == NULL)
			elog(ERROR, "TessFilter target is missing from its child");
		map[target++] = tess_layout_column(&child->layout, found->resno - 1);
	}
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

	if (!tess_plan_child(best_path, custom_plans, 0, &child) ||
		!IsA(child.plan, CustomScan))
		elog(ERROR, "TessFilter expected a batch child");
	take_clauses((CustomScan *) child.plan, actual);
	map_targets(&layout, tlist, &child);
	config.methods = &tess_filter_scan_methods;
	config.layout_policy = TESS_LAYOUT_EXPLICIT;
	config.explicit_layout = &layout;
	config.expressions = actual;
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

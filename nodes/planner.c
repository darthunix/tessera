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
 * The clause the planner evaluates first: order_qual_clauses sorts by
 * cost within security levels, a cheap leakproof clause counting as level
 * zero, and keeps the order of equals. NULL with a pseudoconstant clause:
 * it makes the planner wrap each scan of the relation in a gating Result,
 * which the plan would then find in place of its children.
 */
static RestrictInfo *
first_clause(PlannerInfo *root, RelOptInfo *rel)
{
	RestrictInfo *first = NULL;
	Cost		first_cost = 0;
	Index		first_level = 0;

	foreach_ptr(RestrictInfo, rinfo, rel->baserestrictinfo)
	{
		QualCost	cost;
		Index		level;

		if (rinfo->pseudoconstant)
			return NULL;
		cost_qual_eval_node(&cost, (Node *) rinfo, root);
		level = rinfo->leakproof && cost.per_tuple < 10 * cpu_operator_cost ?
			0 : rinfo->security_level;
		if (first == NULL || level < first_level ||
			(level == first_level && cost.per_tuple < first_cost))
		{
			first = rinfo;
			first_level = level;
			first_cost = cost.per_tuple;
		}
	}
	return first;
}

/* Whether the node has batch work: the first clause is a batch filter. */
static bool
clauses_supported(PlannerInfo *root, RelOptInfo *rel)
{
	RestrictInfo *first = first_clause(root, rel);

	return first != NULL &&
		tess_expr_supports_filter((Node *) first->clause, rel->relid);
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
	PathTarget *target = copy_pathtarget(rel->reltarget);
	List	   *clauses = extract_actual_clauses(rel->baserestrictinfo, false);
	Path	   *child;

	*copy = *seqscan;
	add_new_columns_to_pathtarget(target,
								  pull_var_clause((Node *) clauses,
												  PVC_RECURSE_PLACEHOLDERS));
	copy->pathtarget = set_pathtarget_cost_width(root, target);
	child = tess_batch_scan_path(root, copy);
	return child != NULL ? child : tess_batch_input_path(root, copy);
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
		!relation_supported(root, rel, rte) || !clauses_supported(root, rel))
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
	List	   *prefix = NIL;
	List	   *residual = NIL;

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
	/* The clauses arrive in evaluation order; the first unsupported one ends the batch prefix. */
	foreach_ptr(Node, clause, actual)
	{
		if (residual == NIL && tess_expr_supports_filter(clause, rel->relid))
			prefix = lappend(prefix, clause);
		else
			residual = lappend(residual, clause);
	}
	if (prefix == NIL)
		elog(ERROR, "TessFilter found no batch clause first in the planner's order");
	map_targets(&layout, tlist, &child);
	config.methods = &tess_filter_scan_methods;
	config.layout_policy = TESS_LAYOUT_EXPLICIT;
	config.explicit_layout = &layout;
	config.qual = residual;
	config.expressions = prefix;
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

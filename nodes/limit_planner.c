#include "postgres.h"

#include "optimizer/pathnode.h"
#include "optimizer/planner.h"

#include "tessera/runtime.h"

#include "limit.h"

static create_upper_paths_hook_type previous_create_upper_paths_hook = NULL;

static Plan *limit_plan(PlannerInfo *root, RelOptInfo *rel,
						CustomPath *best_path, List *tlist, List *clauses,
						List *custom_plans);

static const CustomPathMethods limit_path_methods = {
	.CustomName = "TessLimit",
	.PlanCustomPath = limit_plan,
};

/* The node's kind; tessera.enable turns it off with every other batch node. */
const TessNode tess_limit_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_LIMIT_NODE_NAME,
};

const CustomScanMethods tess_limit_scan_methods = {
	.CustomName = "TessLimit",
	.CreateCustomScanState = tess_limit_create_state,
};

/*
 * Whether a batch input is worth a limit above it. A pack pays for itself
 * under a limit only over a scan, whose columns it deforms on request, or
 * over a subquery whose batches it forwards; over any other row-wise node
 * (a sort, an aggregate, a join) it would copy every row the limit reads,
 * and the core limit reads them for free.
 */
static bool
worth_limiting(const Path *child)
{
	const TessNode *pack = tess_runtime_api()->nodes->find(TESS_PACK_NODE_NAME);
	const Path *wrapped;

	if (pack == NULL || tess_path_node(child) != pack)
		return true;
	wrapped = linitial(((const CustomPath *) child)->custom_paths);
	if (wrapped->pathtype == T_SeqScan)
		return true;
	return IsA(wrapped, SubqueryScanPath) &&
		tess_path_node(((const SubqueryScanPath *) wrapped)->subpath) != NULL;
}

/*
 * The node's path in place of the core limit path: the same planner
 * properties above a batch input over the limit's child, carrying the
 * offset and count expressions. NULL when no batch input is possible or
 * worth it.
 */
static CustomPath *
make_limit_path(PlannerInfo *root, LimitPath *limit)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *child = tess_batch_input_path(root, limit->subpath);

	if (child == NULL || !worth_limiting(child))
		return NULL;
	config.template_path = &limit->path;
	config.methods = &limit_path_methods;
	config.node = &tess_limit_node;
	config.children = list_make1(child);
	config.expressions = list_make2(limit->limitOffset, limit->limitCount);
	return tess_path_create(&config);
}

static void
create_upper_paths(PlannerInfo *root, UpperRelationKind stage,
				   RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra)
{
	ListCell   *lc;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel,
										 extra);
	if (!tess_enabled() || stage != UPPERREL_FINAL)
		return;
	foreach(lc, output_rel->pathlist)
	{
		LimitPath  *limit;
		CustomPath *path;

		if (!IsA(lfirst(lc), LimitPath))
			continue;
		limit = lfirst(lc);
		/* WITH TIES stays with the core node. */
		if (limit->limitOption != LIMIT_OPTION_COUNT)
			continue;
		path = make_limit_path(root, limit);
		/*
		 * Replace the path in place rather than through add_path: nothing
		 * is freed, and the limit's child lives on in the input relation.
		 */
		if (path != NULL)
			lfirst(lc) = path;
	}
}

static Plan *
limit_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		   List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);

	/* The limit returns its child's rows: the layout and targets are its. */
	tess_path_get_info(best_path, &info);
	config.methods = &tess_limit_scan_methods;
	config.layout_policy = TESS_LAYOUT_PRESERVE_CHILD;
	config.layout_child = 0;
	config.expressions = info.expressions;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

void
tess_limit_planner_init(void)
{
	RegisterCustomScanMethods(&tess_limit_scan_methods);
	previous_create_upper_paths_hook = create_upper_paths_hook;
	create_upper_paths_hook = create_upper_paths;
}

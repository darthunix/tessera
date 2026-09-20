#include "postgres.h"

#include <string.h>

#include "catalog/pg_class.h"
#include "catalog/pg_type_d.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "fmgr.h"
#include "nodes/makefuncs.h"
#include "nodes/value.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "utils/lsyscache.h"

#include "tessera/planner.h"
#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

PG_FUNCTION_INFO_V1(tessera_test_planner_paths);
PG_FUNCTION_INFO_V1(tessera_test_planner_plans);
PG_FUNCTION_INFO_V1(tessera_test_planner_errors);

/*
 * A forwarding node in the real planner: the hook wraps the sequential
 * scan of every table named planner_*, and the scan returns the child's
 * rows through its own virtual scan slot, so the executor projects
 * through custom_scan_tlist as for any batch node.
 */
typedef struct PlannerTestState
{
	CustomScanState css;
	TessPlanInfo info;
} PlannerTestState;

static set_rel_pathlist_hook_type previous_set_rel_pathlist_hook = NULL;

static const TessNode test_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "tessera.planner_test",
};

static const TessNode unregistered_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "tessera.planner_unregistered",
};

static Node *create_scan_state(CustomScan *cscan);

static const CustomScanMethods scan_methods = {
	.CustomName = "tessera_planner_test",
	.CreateCustomScanState = create_scan_state,
};

static Plan *
plan_custom_path(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
				 List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	Plan	   *child = linitial(custom_plans);

	tess_path_get_info(best_path, &info);
	if (list_length(info.expressions) != 1 || info.node_data == NULL ||
		strcmp(strVal(info.node_data), "marker") != 0)
		elog(ERROR, "Tessera planner test path carries unexpected data");
	config.methods = &scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	/* The child already evaluates the relation's clauses. */
	config.scan_targetlist = child->targetlist;
	config.node_data = info.node_data;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

static const CustomPathMethods path_methods = {
	.CustomName = "tessera_planner_test",
	.PlanCustomPath = plan_custom_path,
};

static const CustomPathMethods other_methods = {
	.CustomName = "other",
};

static void
begin_scan(CustomScanState *css, EState *estate, int eflags)
{
	PlannerTestState *state = (PlannerTestState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);

	state->info = (TessPlanInfo) TESS_STRUCT_INITIALIZER(TessPlanInfo);
	tess_plan_get_info(cscan, &state->info);
	if (state->info.node != &test_node || state->info.nchildren != 1 ||
		state->info.child_names[0] != NULL ||
		state->info.node_data == NULL ||
		strcmp(strVal(state->info.node_data), "marker") != 0)
		elog(ERROR, "Tessera planner test plan carries unexpected data");
	css->custom_ps = list_make1(ExecInitNode(linitial(cscan->custom_plans),
											 estate, eflags));
}

static TupleTableSlot *
fetch_child(ScanState *ss)
{
	TupleTableSlot *slot = ExecProcNode(linitial(((CustomScanState *) ss)->custom_ps));

	if (TupIsNull(slot))
		return NULL;
	return ExecCopySlot(ss->ss_ScanTupleSlot, slot);
}

static bool
recheck(ScanState *ss, TupleTableSlot *slot)
{
	return true;
}

static TupleTableSlot *
exec_scan(CustomScanState *css)
{
	return ExecScan(&css->ss, fetch_child, recheck);
}

static void
end_scan(CustomScanState *css)
{
	ExecEndNode(linitial(css->custom_ps));
}

static void
rescan(CustomScanState *css)
{
	ExecReScan(linitial(css->custom_ps));
}

static void
explain_scan(CustomScanState *css, List *ancestors, ExplainState *es)
{
	PlannerTestState *state = (PlannerTestState *) css;

	ExplainPropertyText("Node", state->info.node->name, es);
	ExplainPropertyInteger("Layout Columns", NULL,
						   state->info.layout.ncolumns, es);
}

static const CustomExecMethods exec_methods = {
	.CustomName = "tessera_planner_test",
	.BeginCustomScan = begin_scan,
	.ExecCustomScan = exec_scan,
	.EndCustomScan = end_scan,
	.ReScanCustomScan = rescan,
	.ExplainCustomScan = explain_scan,
};

static Node *
create_scan_state(CustomScan *cscan)
{
	PlannerTestState *state = (PlannerTestState *)
		newNode(sizeof(PlannerTestState), T_CustomScanState);

	state->css.methods = &exec_methods;
	return (Node *) state;
}

static void
set_rel_pathlist(PlannerInfo *root, RelOptInfo *rel, Index rti,
				 RangeTblEntry *rte)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	const char *name;
	Path	   *seqscan = NULL;
	Path	   *child;
	Path		template;

	if (previous_set_rel_pathlist_hook != NULL)
		previous_set_rel_pathlist_hook(root, rel, rti, rte);
	if (!*tess_runtime_api()->settings->enable)
		return;
	if (rte->rtekind != RTE_RELATION || rte->relkind != RELKIND_RELATION)
		return;
	name = get_rel_name(rte->relid);
	if (name == NULL || strncmp(name, "planner_", 8) != 0)
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
	/* add_path frees the dominated core path; the child is a copy. */
	child = makeNode(Path);
	*child = *seqscan;
	template = *seqscan;
	template.total_cost *= 0.5;
	config.template_path = &template;
	config.methods = &path_methods;
	config.node = &test_node;
	config.children = list_make1(child);
	config.expressions = list_make1(makeConst(INT4OID, -1, InvalidOid, 4,
											  Int32GetDatum(42), false, true));
	config.node_data = (Node *) makeString("marker");
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	add_path(rel, (Path *) tess_path_create(&config));
}

void
_PG_init(void)
{
	tess_runtime_api()->nodes->add(&test_node);
	RegisterCustomScanMethods(&scan_methods);
	previous_set_rel_pathlist_hook = set_rel_pathlist_hook;
	set_rel_pathlist_hook = set_rel_pathlist;
}

/* A stand-in for a core path with the properties the helper copies. */
static Path *
make_template(void)
{
	Path	   *path = makeNode(Path);

	path->pathtype = T_SeqScan;
	path->rows = 10;
	path->startup_cost = 1;
	path->total_cost = 20;
	path->parallel_safe = true;
	return path;
}

static TessPathConfig
make_path_config(const Path *template)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);

	config.template_path = template;
	config.methods = &path_methods;
	config.node = &test_node;
	return config;
}

/* A custom path of another provider, with its own private data. */
static CustomPath *
make_foreign_path(void)
{
	CustomPath *path = makeNode(CustomPath);

	path->path.pathtype = T_CustomScan;
	path->methods = &other_methods;
	path->custom_private = list_make1(makeInteger(1));
	return path;
}

Datum
tessera_test_planner_paths(PG_FUNCTION_ARGS)
{
	Path	   *template = make_template();
	Path	   *child = make_template();
	TessPathConfig config = make_path_config(template);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	CustomPath *path;
	bool		result;

	config.children = list_make1(child);
	config.expressions = list_make1(makeConst(INT4OID, -1, InvalidOid, 4,
											  Int32GetDatum(42), false, true));
	config.node_data = (Node *) makeString("marker");
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	path = tess_path_create(&config);
	result = IsA(path, CustomPath) && path->path.pathtype == T_CustomScan &&
		path->path.rows == 10 && path->path.startup_cost == 1 &&
		path->path.total_cost == 20 && path->path.parallel_safe &&
		path->flags == CUSTOMPATH_SUPPORT_PROJECTION &&
		path->methods == &path_methods &&
		list_length(path->custom_paths) == 1 &&
		linitial(path->custom_paths) == child;
	result &= tess_path_matches(&path->path, &path_methods) &&
		!tess_path_matches(&path->path, &other_methods) &&
		!tess_path_matches(template, &path_methods);
	result &= tess_path_node(&path->path) == &test_node &&
		tess_path_node(template) == NULL && tess_path_node(NULL) == NULL &&
		tess_path_node(&make_foreign_path()->path) == NULL;
	/* Without a pack node loaded, only a batch path is a batch input. */
	result &= tess_batch_input_path(NULL, &path->path) == &path->path &&
		tess_batch_input_path(NULL, template) == NULL;
	tess_path_get_info(path, &info);
	result &= info.node == &test_node &&
		list_length(info.expressions) == 1 &&
		IsA(linitial(info.expressions), Const) &&
		DatumGetInt32(castNode(Const, linitial(info.expressions))->constvalue) == 42 &&
		info.node_data != NULL && IsA(info.node_data, String) &&
		strcmp(strVal(info.node_data), "marker") == 0;
	/* Nothing carried reads back as nothing. */
	config = make_path_config(template);
	path = tess_path_create(&config);
	tess_path_get_info(path, &info);
	result &= info.expressions == NIL && info.node_data == NULL &&
		path->custom_paths == NIL && path->custom_restrictinfo == NIL;
	PG_RETURN_BOOL(result);
}

/* Integer Vars of one relation, as many as requested. */
static List *
make_targetlist(int natts)
{
	List	   *targetlist = NIL;

	for (int attno = 1; attno <= natts; attno++)
	{
		Var		   *var = makeVar(1, attno, INT4OID, -1, InvalidOid, 0);

		targetlist = lappend(targetlist,
							 makeTargetEntry((Expr *) var, attno, "c", false));
	}
	return targetlist;
}

static TessPlanConfig
make_plan_config(TessLayoutPolicy policy)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);

	config.methods = &scan_methods;
	config.layout_policy = policy;
	return config;
}

/* True when the info describes a batch child followed by a row child. */
static bool
check_children(const Plan *plan, int ncolumns)
{
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);

	tess_plan_get_info(castNode(CustomScan, plan), &info);
	return info.nchildren == 2 && info.child_names[0] != NULL &&
		strcmp(info.child_names[0], test_node.name) == 0 &&
		info.child_names[1] == NULL && info.layout.ncolumns == ncolumns &&
		list_length(castNode(CustomScan, plan)->custom_plans) == 2;
}

Datum
tessera_test_planner_plans(PG_FUNCTION_ARGS)
{
	Path	   *template = make_template();
	TessPathConfig path_config = make_path_config(template);
	TessPlanConfig config = make_plan_config(TESS_LAYOUT_EXPLICIT);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	TessLayout	explicit = TESS_STRUCT_INITIALIZER(TessLayout);
	int			map[2] = {2, 0};
	List	   *tlist = make_targetlist(2);
	CustomPath *path;
	CustomPath *parent;
	CustomScan *scan;
	Plan	   *plan;
	Plan	   *batch_child;
	SeqScan    *row_child;
	bool		result;

	/* An explicit layout: three batch columns, two targets, reordered. */
	path = tess_path_create(&path_config);
	explicit.ncolumns = 3;
	explicit.ntargets = 2;
	explicit.target_columns = map;
	config.explicit_layout = &explicit;
	config.expressions = list_make1(makeConst(INT4OID, -1, InvalidOid, 4,
											  Int32GetDatum(42), false, true));
	config.node_data = (Node *) makeString("plan");
	plan = tess_plan_create(path, tlist, NIL, &config);
	scan = castNode(CustomScan, plan);
	result = scan->methods == &scan_methods && scan->scan.scanrelid == 0 &&
		list_length(plan->targetlist) == 2 && plan->targetlist != tlist &&
		list_length(scan->custom_scan_tlist) == 2 &&
		scan->custom_scan_tlist != tlist && scan->custom_plans == NIL &&
		list_length(scan->custom_exprs) == 1 && plan->qual == NIL;
	map[0] = 1;					/* the layout was copied */
	tess_plan_get_layout(plan, &layout);
	result &= layout.ncolumns == 3 && layout.ntargets == 2 &&
		layout.target_columns != NULL && layout.target_columns[0] == 2 &&
		layout.target_columns[1] == 0;
	tess_plan_get_info(scan, &info);
	result &= info.node == &test_node && info.nchildren == 0 &&
		info.node_data != NULL && IsA(info.node_data, String) &&
		strcmp(strVal(info.node_data), "plan") == 0 &&
		info.layout.ncolumns == 3 && info.layout.target_columns[1] == 0;
	batch_child = plan;

	/*
	 * A dense layout follows the final target list: PostgreSQL passes none
	 * when it will install a projection after PlanCustomPath.
	 */
	config = make_plan_config(TESS_LAYOUT_DENSE);
	config.scan_targetlist = tlist;
	plan = tess_plan_create(path, NIL, NIL, &config);
	scan = castNode(CustomScan, plan);
	result &= plan->targetlist == NIL &&
		list_length(scan->custom_scan_tlist) == 2;
	plan->targetlist = make_targetlist(3);
	tess_plan_get_layout(plan, &layout);
	result &= layout.ncolumns == 3 && layout.ntargets == 3 &&
		layout.target_columns == NULL;

	/* Children are recorded by kind, through a cached-plan copy and a
	 * parallel worker's text round trip alike. */
	row_child = makeNode(SeqScan);
	row_child->scan.scanrelid = 1;
	row_child->scan.plan.targetlist = make_targetlist(2);
	path_config.children = list_make2(path, make_template());
	parent = tess_path_create(&path_config);
	config = make_plan_config(TESS_LAYOUT_DENSE);
	plan = tess_plan_create(parent, tlist, list_make2(batch_child, row_child),
							&config);
	result &= check_children(plan, 2) && check_children(copyObject(plan), 2) &&
		check_children(stringToNode(nodeToString(plan)), 2);

	/* A parent inspects its children and may keep a batch child's layout. */
	{
		TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);
		List	   *plans = list_make2(batch_child, row_child);

		result &= tess_plan_child(parent, plans, 0, &child) &&
			child.path == (Path *) path && child.plan == batch_child &&
			child.node == &test_node && child.layout.ncolumns == 3 &&
			child.layout.ntargets == 2 && child.layout.target_columns[0] == 2;
		result &= !tess_plan_child(parent, plans, 1, &child) &&
			child.path == lsecond(parent->custom_paths) &&
			child.plan == (Plan *) row_child && child.node == NULL &&
			child.layout.ncolumns == 0 && child.layout.target_columns == NULL;
		config = make_plan_config(TESS_LAYOUT_PRESERVE_CHILD);
		config.layout_child = 0;
		plan = tess_plan_create(parent, tlist, plans, &config);
		scan = castNode(CustomScan, plan);
		tess_plan_get_layout(plan, &layout);
		result &= layout.ncolumns == 3 && layout.ntargets == 2 &&
			layout.target_columns[0] == 2 && layout.target_columns[1] == 0 &&
			list_length(scan->custom_scan_tlist) == 2 &&
			scan->custom_scan_tlist != batch_child->targetlist &&
			check_children(plan, 3);
	}
	PG_RETURN_BOOL(result);
}

Datum
tessera_test_planner_errors(PG_FUNCTION_ARGS)
{
	int			kind = PG_GETARG_INT32(0);
	Path	   *template = make_template();
	TessPathConfig config = make_path_config(template);
	TessPlanConfig plan_config = make_plan_config(TESS_LAYOUT_EXPLICIT);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanInfo plan_info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	List	   *tlist = make_targetlist(2);
	int			map[1] = {5};
	TessNode   *removed;
	CustomPath *path;
	CustomScan *scan;
	Plan	   *plan;

	layout.ncolumns = 1;
	layout.ntargets = 1;
	plan_config.explicit_layout = &layout;
	switch (kind)
	{
		case 0:
			config.struct_size = 1;
			tess_path_create(&config);
			break;
		case 1:
			config.template_path = NULL;
			tess_path_create(&config);
			break;
		case 2:
			config.methods = NULL;
			tess_path_create(&config);
			break;
		case 3:
			config.node = &unregistered_node;
			tess_path_create(&config);
			break;
		case 4:
			template->param_info = makeNode(ParamPathInfo);
			tess_path_create(&config);
			break;
		case 5:
			config.children = list_make1(NULL);
			tess_path_create(&config);
			break;
		case 6:
			tess_path_get_info(make_foreign_path(), &info);
			break;
		case 7:
			info.struct_size = 1;
			tess_path_get_info(tess_path_create(&config), &info);
			break;
		case 8:
			tess_path_get_info((CustomPath *) template, &info);
			break;
		case 9:
			/* The node module of a path was unloaded. */
			removed = palloc0_object(TessNode);
			removed->abi_version = TESS_NODE_ABI_VERSION;
			removed->struct_size = sizeof(*removed);
			removed->name = "tessera.planner_removed";
			tess_runtime_api()->nodes->add(removed);
			config.node = removed;
			path = tess_path_create(&config);
			tess_runtime_api()->nodes->remove(removed);
			tess_path_node(&path->path);
			break;
		case 10:
			plan_config.struct_size = 1;
			tess_plan_create(tess_path_create(&config), tlist, NIL, &plan_config);
			break;
		case 11:
			plan_config.methods = NULL;
			tess_plan_create(tess_path_create(&config), tlist, NIL, &plan_config);
			break;
		case 12:
			plan_config.layout_policy = 7;
			tess_plan_create(tess_path_create(&config), tlist, NIL, &plan_config);
			break;
		case 13:
			plan_config.explicit_layout = NULL;
			tess_plan_create(tess_path_create(&config), tlist, NIL, &plan_config);
			break;
		case 14:
			/* One target described, two in the list. */
			tess_plan_create(tess_path_create(&config), tlist, NIL, &plan_config);
			break;
		case 15:
			layout.target_columns = map;
			tess_plan_create(tess_path_create(&config), make_targetlist(1),
							 NIL, &plan_config);
			break;
		case 16:
			config.children = list_make1(make_template());
			tess_plan_create(tess_path_create(&config), tlist, NIL, &plan_config);
			break;
		case 17:
			config.children = list_make1(make_template());
			tess_plan_create(tess_path_create(&config), tlist, list_make1(NULL),
							 &plan_config);
			break;
		case 18:
			tess_plan_create(make_foreign_path(), tlist, NIL, &plan_config);
			break;
		case 19:
			tess_plan_get_layout((Plan *) makeNode(SeqScan), &layout);
			break;
		case 20:
			scan = makeNode(CustomScan);
			scan->custom_private = list_make1(makeInteger(1));
			tess_plan_get_info(scan, &plan_info);
			break;
		case 21:
			plan_info.struct_size = 1;
			tess_plan_get_info((CustomScan *) tess_plan_create(
				tess_path_create(&config), make_targetlist(1), NIL,
				&plan_config), &plan_info);
			break;
		case 22:
			/* An explicit layout does not follow a replaced target list. */
			plan = tess_plan_create(tess_path_create(&config),
									make_targetlist(1), NIL, &plan_config);
			plan->targetlist = tlist;
			tess_plan_get_layout(plan, &layout);
			break;
		case 23:
			{
				TessLayout	result = TESS_STRUCT_INITIALIZER(TessLayout);

				result.struct_size = 1;
				tess_plan_get_layout(tess_plan_create(tess_path_create(&config),
													  make_targetlist(1), NIL,
													  &plan_config), &result);
				break;
			}
		case 24:
			{
				TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);

				child.struct_size = 1;
				tess_plan_child(tess_path_create(&config), NIL, 0, &child);
				break;
			}
		case 25:
			{
				TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);

				config.children = list_make1(make_template());
				tess_plan_child(tess_path_create(&config),
								list_make1(makeNode(SeqScan)), 1, &child);
				break;
			}
		case 26:
			config.children = list_make1(make_template());
			plan_config = make_plan_config(TESS_LAYOUT_PRESERVE_CHILD);
			tess_plan_create(tess_path_create(&config), tlist,
							 list_make1(makeNode(SeqScan)), &plan_config);
			break;
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	PG_RETURN_VOID();
}

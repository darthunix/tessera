#include "postgres.h"

#include <string.h>

#include "catalog/pg_type_d.h"
#include "fmgr.h"
#include "nodes/makefuncs.h"
#include "nodes/value.h"

#include "tessera/planner.h"
#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

PG_FUNCTION_INFO_V1(tessera_test_planner_paths);
PG_FUNCTION_INFO_V1(tessera_test_planner_errors);

static const TessNode test_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "tessera.planner_test",
};

static const TessNode unregistered_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = "tessera.planner_unregistered",
};

static Plan *
plan_custom_path(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
				 List *tlist, List *clauses, List *custom_plans)
{
	elog(ERROR, "Tessera planner test path is not planned here");
}

static const CustomPathMethods path_methods = {
	.CustomName = "tessera_planner_test",
	.PlanCustomPath = plan_custom_path,
};

static const CustomPathMethods other_methods = {
	.CustomName = "other",
};

void
_PG_init(void)
{
	tess_runtime_api()->nodes->add(&test_node);
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

Datum
tessera_test_planner_errors(PG_FUNCTION_ARGS)
{
	int			kind = PG_GETARG_INT32(0);
	Path	   *template = make_template();
	TessPathConfig config = make_path_config(template);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessNode   *removed;
	CustomPath *path;

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
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	PG_RETURN_VOID();
}

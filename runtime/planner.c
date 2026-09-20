#include "postgres.h"

#include "nodes/makefuncs.h"
#include "nodes/value.h"

#include "tessera/plan.h"
#include "tessera/planner.h"
#include "tessera/runtime.h"

#define PATH_DATA_KIND "tessera.path"
#define PLAN_DATA_KIND "tessera.plan"
#define DATA_VERSION 1

/* The registered node of this name; an unloaded node module is an error. */
static const TessNode *
find_node(const char *name)
{
	const TessNode *node = tess_runtime_api()->nodes->find(name);

	if (node == NULL)
		elog(ERROR, "Tessera node \"%s\" is not registered", name);
	return node;
}

static void
check_path_config(const TessPathConfig *config)
{
	if (config == NULL || config->struct_size < TESS_PATH_CONFIG_MIN_SIZE)
		elog(ERROR, "Tessera received an incompatible path configuration");
	if (config->template_path == NULL || config->methods == NULL)
		elog(ERROR, "Tessera path requires a template and methods");
	if (config->node == NULL || config->node->name == NULL ||
		tess_runtime_api()->nodes->find(config->node->name) != config->node)
		elog(ERROR, "Tessera path requires a registered node");
	/* Batch paths are never the inner side of a nested loop. */
	if (config->template_path->param_info != NULL)
		elog(ERROR, "Tessera path cannot be parameterized");
	foreach_ptr(Path, child, config->children)
	{
		if (child == NULL)
			elog(ERROR, "Tessera path received an invalid child");
	}
}

CustomPath *
tess_path_create(const TessPathConfig *config)
{
	TessPlanWriter *writer;
	CustomPath *path;

	check_path_config(config);
	path = makeNode(CustomPath);
	path->path = *config->template_path;
	NodeSetTag(path, T_CustomPath);
	path->path.pathtype = T_CustomScan;
	path->flags = config->flags;
	path->custom_paths = list_copy(config->children);
	path->custom_restrictinfo = copyObject(config->restrictinfo);
	writer = tess_plan_writer_create(PATH_DATA_KIND, DATA_VERSION);
	tess_plan_write_string(writer, "node", config->node->name);
	tess_plan_write_list(writer, "expressions", config->expressions);
	tess_plan_write_node(writer, "node_data", config->node_data);
	path->custom_private = tess_plan_writer_finish(writer);
	path->methods = config->methods;
	return path;
}

bool
tess_path_matches(const Path *path, const CustomPathMethods *methods)
{
	return path != NULL && methods != NULL && IsA(path, CustomPath) &&
		((const CustomPath *) path)->methods == methods;
}

const TessNode *
tess_path_node(const Path *path)
{
	const CustomPath *custom = (const CustomPath *) path;
	const char *kind;
	TessPlanReader *reader;

	if (path == NULL || !IsA(path, CustomPath))
		return NULL;
	kind = tess_plan_data_kind(custom->custom_private);
	if (kind == NULL || strcmp(kind, PATH_DATA_KIND) != 0)
		return NULL;
	reader = tess_plan_reader_create(custom->custom_private, PATH_DATA_KIND,
									 DATA_VERSION);
	return find_node(tess_plan_read_string(reader, "node"));
}

void
tess_path_get_info(const CustomPath *path, TessPathInfo *result)
{
	TessPlanReader *reader;

	if (result == NULL || result->struct_size < TESS_PATH_INFO_MIN_SIZE)
		elog(ERROR, "Tessera received an incompatible path result");
	if (path == NULL || !IsA(path, CustomPath))
		elog(ERROR, "Tessera expected a custom path");
	reader = tess_plan_reader_create(path->custom_private, PATH_DATA_KIND,
									 DATA_VERSION);
	result->node = find_node(tess_plan_read_string(reader, "node"));
	result->expressions = tess_plan_read_list(reader, "expressions");
	result->node_data = tess_plan_read_node(reader, "node_data");
	tess_plan_reader_finish(reader);
}

Path *
tess_batch_input_path(PlannerInfo *root, Path *path)
{
	const TessNode *pack;
	CustomPath *wrapped;

	if (path == NULL)
		elog(ERROR, "Tessera batch input requires a path");
	if (tess_path_node(path) != NULL)
		return path;
	if (path->param_info != NULL)
		return NULL;
	pack = tess_runtime_api()->nodes->find(TESS_PACK_NODE_NAME);
	if (pack == NULL || !TESS_ABI_HAS_FIELD(pack, TessNode, wrap_rows) ||
		pack->wrap_rows == NULL)
		return NULL;
	wrapped = pack->wrap_rows(root, path);
	if (wrapped == NULL || tess_path_node(&wrapped->path) != pack)
		elog(ERROR, "Tessera pack node returned a foreign path");
	return &wrapped->path;
}

static void
check_layout(const TessLayout *layout)
{
	if (layout == NULL || layout->struct_size < TESS_LAYOUT_MIN_SIZE)
		elog(ERROR, "Tessera received an incompatible layout");
	if (layout->ncolumns < 0 || layout->ntargets < 0)
		elog(ERROR, "Tessera layout has a negative size");
	for (int target = 0; layout->target_columns != NULL &&
		 target < layout->ntargets; target++)
	{
		int			column = layout->target_columns[target];

		if (column < -1 || column >= layout->ncolumns)
			elog(ERROR, "Tessera layout maps target %d to an invalid column %d",
				 target, column);
	}
}

/* Copy into a caller-initialized layout; the map is allocated here. */
static void
copy_layout(const TessLayout *source, TessLayout *result)
{
	check_layout(source);
	result->ncolumns = source->ncolumns;
	result->ntargets = source->ntargets;
	result->target_columns = NULL;
	if (source->target_columns != NULL)
	{
		int		   *columns = palloc_array(int, source->ntargets);

		memcpy(columns, source->target_columns,
			   sizeof(*columns) * source->ntargets);
		result->target_columns = columns;
	}
}

static void
check_plan_config(const TessPlanConfig *config)
{
	if (config == NULL || config->struct_size < TESS_PLAN_CONFIG_MIN_SIZE)
		elog(ERROR, "Tessera received an incompatible plan configuration");
	if (config->methods == NULL)
		elog(ERROR, "Tessera plan requires scan methods");
	if (config->layout_policy < TESS_LAYOUT_DENSE ||
		config->layout_policy > TESS_LAYOUT_PRESERVE_CHILD)
		elog(ERROR, "Tessera plan received an invalid layout policy");
	if (config->layout_policy == TESS_LAYOUT_EXPLICIT &&
		config->explicit_layout == NULL)
		elog(ERROR, "Tessera plan requires an explicit output layout");
}

static void
check_children(const CustomPath *path, const List *child_plans)
{
	if (path == NULL || !IsA(path, CustomPath))
		elog(ERROR, "Tessera expected a custom path");
	if (list_length(path->custom_paths) != list_length(child_plans))
		elog(ERROR, "Tessera path and plan child counts differ");
	foreach_ptr(Plan, plan, child_plans)
	{
		if (plan == NULL)
			elog(ERROR, "Tessera received an invalid child plan");
	}
}

bool
tess_plan_child(const CustomPath *path, const List *child_plans, int index,
				TessPlanChild *result)
{
	if (result == NULL || result->struct_size < TESS_PLAN_CHILD_MIN_SIZE)
		elog(ERROR, "Tessera received an incompatible plan child result");
	check_children(path, child_plans);
	if (index < 0 || index >= list_length(child_plans))
		elog(ERROR, "Tessera plan child index %d is out of range", index);
	result->path = list_nth(path->custom_paths, index);
	result->plan = list_nth(child_plans, index);
	result->node = tess_path_node(result->path);
	result->layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	if (result->node == NULL)
		return false;
	tess_plan_get_layout(result->plan, &result->layout);
	return true;
}

Plan *
tess_plan_create(CustomPath *path, List *targetlist, List *child_plans,
				 const TessPlanConfig *config)
{
	TessPathInfo path_info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);
	TessPlanWriter *writer;
	CustomScan *scan;
	List	   *child_names = NIL;
	List	   *target_columns = NIL;
	List	   *scan_targetlist;

	check_plan_config(config);
	check_children(path, child_plans);
	tess_path_get_info(path, &path_info);
	foreach_ptr(Path, child, path->custom_paths)
	{
		const TessNode *node = tess_path_node(child);

		child_names = lappend(child_names, node == NULL ? NULL :
							  makeString(pstrdup(node->name)));
	}
	switch (config->layout_policy)
	{
		case TESS_LAYOUT_DENSE:
			layout.ncolumns = list_length(targetlist);
			layout.ntargets = layout.ncolumns;
			break;
		case TESS_LAYOUT_EXPLICIT:
			copy_layout(config->explicit_layout, &layout);
			break;
		case TESS_LAYOUT_PRESERVE_CHILD:
			{
				TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);

				if (!tess_plan_child(path, child_plans, config->layout_child,
									 &child))
					elog(ERROR, "Tessera cannot preserve the layout of a row-producing child");
				layout = child.layout;
				break;
			}
	}
	if (layout.ntargets != list_length(targetlist))
		elog(ERROR, "Tessera output layout does not match its target list");
	for (int target = 0; layout.target_columns != NULL &&
		 target < layout.ntargets; target++)
		target_columns = lappend_int(target_columns,
									 layout.target_columns[target]);
	if (config->scan_targetlist != NIL)
		scan_targetlist = copyObject(config->scan_targetlist);
	else if (config->layout_policy == TESS_LAYOUT_PRESERVE_CHILD)
		scan_targetlist = copyObject(((Plan *)
			list_nth(child_plans, config->layout_child))->targetlist);
	else
		scan_targetlist = copyObject(targetlist);

	scan = makeNode(CustomScan);
	scan->methods = config->methods;
	scan->flags = path->flags;
	scan->scan.scanrelid = config->scanrelid;
	scan->scan.plan.targetlist = copyObject(targetlist);
	scan->scan.plan.qual = copyObject(config->qual);
	scan->custom_plans = child_plans;
	scan->custom_exprs = copyObject(config->expressions);
	scan->custom_scan_tlist = scan_targetlist;
	writer = tess_plan_writer_create(PLAN_DATA_KIND, DATA_VERSION);
	tess_plan_write_string(writer, "node", path_info.node->name);
	tess_plan_write_list(writer, "child_names", child_names);
	tess_plan_write_int(writer, "layout_policy", config->layout_policy);
	tess_plan_write_int(writer, "ncolumns", layout.ncolumns);
	tess_plan_write_int(writer, "ntargets", layout.ntargets);
	tess_plan_write_int(writer, "mapped", layout.target_columns != NULL);
	tess_plan_write_int_list(writer, "target_columns", target_columns);
	tess_plan_write_node(writer, "node_data", config->node_data);
	scan->custom_private = tess_plan_writer_finish(writer);
	return &scan->scan.plan;
}

void
tess_plan_get_info(const CustomScan *scan, TessPlanInfo *result)
{
	TessPlanReader *reader;
	List	   *child_names;
	List	   *target_columns;
	int			layout_policy;
	bool		mapped;
	int			child = 0;

	if (result == NULL || result->struct_size < TESS_PLAN_INFO_MIN_SIZE)
		elog(ERROR, "Tessera received an incompatible plan result");
	if (scan == NULL || !IsA(scan, CustomScan))
		elog(ERROR, "Tessera expected a custom plan");
	reader = tess_plan_reader_create(scan->custom_private, PLAN_DATA_KIND,
									 DATA_VERSION);
	result->node = find_node(tess_plan_read_string(reader, "node"));
	child_names = tess_plan_read_list(reader, "child_names");
	result->nchildren = list_length(child_names);
	if (result->nchildren != list_length(scan->custom_plans))
		elog(ERROR, "Tessera plan child names do not match its children");
	result->child_names = palloc0_array(const char *, result->nchildren);
	foreach_ptr(Node, value, child_names)
	{
		if (value != NULL && !IsA(value, String))
			elog(ERROR, "Tessera plan received an invalid child name");
		result->child_names[child++] = value == NULL ? NULL : strVal(value);
	}
	layout_policy = tess_plan_read_int(reader, "layout_policy");
	if (layout_policy < TESS_LAYOUT_DENSE ||
		layout_policy > TESS_LAYOUT_PRESERVE_CHILD)
		elog(ERROR, "Tessera plan received an invalid layout policy");
	result->layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	result->layout.ncolumns = tess_plan_read_int(reader, "ncolumns");
	result->layout.ntargets = tess_plan_read_int(reader, "ntargets");
	mapped = tess_plan_read_int(reader, "mapped") != 0;
	target_columns = tess_plan_read_int_list(reader, "target_columns");
	if (mapped)
	{
		int		   *columns;
		int			target = 0;

		if (list_length(target_columns) != result->layout.ntargets)
			elog(ERROR, "Tessera plan received an invalid target map");
		columns = palloc_array(int, result->layout.ntargets);
		foreach_int(column, target_columns)
			columns[target++] = column;
		result->layout.target_columns = columns;
	}
	else if (target_columns != NIL)
		elog(ERROR, "Tessera identity layout carries a target map");
	result->node_data = tess_plan_read_node(reader, "node_data");
	tess_plan_reader_finish(reader);
	if (layout_policy == TESS_LAYOUT_DENSE)
	{
		/* PostgreSQL may replace a projection after PlanCustomPath. */
		if (result->layout.target_columns != NULL)
			pfree((void *) result->layout.target_columns);
		result->layout.ncolumns = list_length(scan->scan.plan.targetlist);
		result->layout.ntargets = result->layout.ncolumns;
		result->layout.target_columns = NULL;
	}
	check_layout(&result->layout);
	if (result->layout.ntargets != list_length(scan->scan.plan.targetlist))
		elog(ERROR, "Tessera plan layout does not match its target list");
}

void
tess_plan_get_layout(const Plan *plan, TessLayout *result)
{
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);

	if (result == NULL || result->struct_size < TESS_LAYOUT_MIN_SIZE)
		elog(ERROR, "Tessera received an incompatible output layout");
	if (plan == NULL || !IsA(plan, CustomScan))
		elog(ERROR, "Tessera expected a custom plan");
	tess_plan_get_info((const CustomScan *) plan, &info);
	result->ncolumns = info.layout.ncolumns;
	result->ntargets = info.layout.ntargets;
	result->target_columns = info.layout.target_columns;
	pfree(info.child_names);
}

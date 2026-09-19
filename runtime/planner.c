#include "postgres.h"

#include "nodes/makefuncs.h"
#include "nodes/value.h"

#include "tessera/plan.h"
#include "tessera/planner.h"
#include "tessera/runtime.h"

#define PATH_DATA_KIND "tessera.path"
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

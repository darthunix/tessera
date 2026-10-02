#include "postgres.h"

#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "nodes/value.h"
#include "optimizer/tlist.h"

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

/*
 * Whether the planner will gate every scan of the path's relation with a
 * Result for a pseudoconstant clause: it stands between a batch parent
 * and its child, which the parent cannot read through.
 */
static bool
gated(PlannerInfo *root, const Path *path)
{
	if (root == NULL || path->parent == NULL || !root->hasPseudoConstantQuals)
		return false;
	foreach_ptr(RestrictInfo, rinfo, path->parent->baserestrictinfo)
	{
		if (rinfo->pseudoconstant)
			return true;
	}
	return false;
}

Path *
tess_batch_scan_path(PlannerInfo *root, Path *path)
{
	const TessNode *scan;
	CustomPath *built;

	if (path == NULL)
		elog(ERROR, "Tessera batch scan requires a path");
	if (path->pathtype != T_SeqScan || path->param_info != NULL ||
		gated(root, path))
		return NULL;
	scan = tess_runtime_api()->nodes->find(TESS_HEAP_SCAN_NODE_NAME);
	if (scan == NULL || !TESS_ABI_HAS_FIELD(scan, TessNode, scan_rows) ||
		scan->scan_rows == NULL)
		return NULL;
	built = scan->scan_rows(root, path);
	if (built == NULL)
		return NULL;
	if (tess_path_node(&built->path) != scan)
		elog(ERROR, "Tessera batch scan node returned a foreign path");
	return &built->path;
}

Path *
tess_batch_input_path(PlannerInfo *root, Path *path)
{
	const TessNode *pack;
	CustomPath *wrapped;

	if (path == NULL)
		elog(ERROR, "Tessera batch input requires a path");
	/*
	 * A projection the planner put above a batch node that projects itself
	 * is the node's to compute: a copy of the node's path takes the target
	 * and stands in for the projection path, which would otherwise hide it
	 * behind a Result. The path itself may stand under other parents, in
	 * its relation's list: its target stays.
	 */
	if (IsA(path, ProjectionPath) && ((ProjectionPath *) path)->dummypp)
	{
		ProjectionPath *projection = (ProjectionPath *) path;
		Path	   *subpath = projection->subpath;

		if (tess_path_node(subpath) != NULL)
		{
			CustomPath *copy = makeNode(CustomPath);

			*copy = *castNode(CustomPath, subpath);
			copy->path.pathtarget = projection->path.pathtarget;
			path = &copy->path;
		}
		else if (subpath->pathtype == T_SeqScan && subpath->param_info == NULL)
		{
			/* A native scan may compute it too; the core path is left alone. */
			Path	   *copy = makeNode(Path);

			*copy = *subpath;
			copy->pathtarget = projection->path.pathtarget;
			path = copy;
		}
	}
	if (tess_path_node(path) != NULL)
		return path;
	if (path->param_info != NULL || gated(root, path))
		return NULL;
	/* An Append's children are read in turn as batches when a node kind can. */
	if (IsA(path, AppendPath))
	{
		const TessNode *append = tess_runtime_api()->nodes->find(TESS_APPEND_NODE_NAME);

		if (append != NULL && TESS_ABI_HAS_FIELD(append, TessNode, wrap_append) &&
			append->wrap_append != NULL)
		{
			CustomPath *built = append->wrap_append(root, path);

			if (built != NULL)
			{
				if (tess_path_node(&built->path) != append)
					elog(ERROR, "Tessera append node returned a foreign path");
				return &built->path;
			}
		}
	}
	/* A relation without clauses is read natively when a node kind can. */
	if (path->parent != NULL && path->parent->baserestrictinfo == NIL)
	{
		Path	   *scan = tess_batch_scan_path(root, path);

		if (scan != NULL)
			return scan;
	}
	pack = tess_runtime_api()->nodes->find(TESS_PACK_NODE_NAME);
	if (pack == NULL || !TESS_ABI_HAS_FIELD(pack, TessNode, wrap_rows) ||
		pack->wrap_rows == NULL)
		return NULL;
	wrapped = pack->wrap_rows(root, path);
	if (wrapped == NULL || tess_path_node(&wrapped->path) != pack)
		elog(ERROR, "Tessera pack node returned a foreign path");
	return &wrapped->path;
}

static Node *
setop_columns_mutator(Node *node, void *context)
{
	const List *targets = (const List *) context;

	if (node == NULL)
		return NULL;
	if (IsA(node, Var) && ((Var *) node)->varno == 0 && ((Var *) node)->varlevelsup == 0)
	{
		Var		   *var = (Var *) node;

		Node	   *target;

		if (var->varattno < 1 || var->varattno > list_length(targets))
			elog(ERROR, "Tessera found no child target for column %d of a set operation",
				 var->varattno);
		target = (Node *) ((TargetEntry *) list_nth(targets, var->varattno - 1))->expr;
		/* setrefs would keep it, not read the child: see tess_path_setop_constant. */
		if (IsA(target, Const) || IsA(target, Param))
			elog(ERROR, "Tessera cannot read column %d of a set operation, a constant of its first branch",
				 var->varattno);
		return (Node *) copyObject(target);
	}
	return expression_tree_mutator(node, setop_columns_mutator, context);
}

static bool
setop_columns_walker(Node *node, void *context)
{
	if (node == NULL)
		return false;
	if (IsA(node, Var))
		return ((Var *) node)->varno == 0 && ((Var *) node)->varlevelsup == 0;
	return expression_tree_walker(node, setop_columns_walker, context);
}

bool
tess_plan_has_setop_columns(Node *node)
{
	return setop_columns_walker(node, NULL);
}

/* The first plan's first child: the one whose targets the plan's show. */
static const Plan *
first_child(const Plan *plan)
{
	if (IsA(plan, Append))
		return ((Append *) plan)->appendplans != NIL ?
			linitial(((Append *) plan)->appendplans) : NULL;
	if (IsA(plan, MergeAppend))
		return ((MergeAppend *) plan)->mergeplans != NIL ?
			linitial(((MergeAppend *) plan)->mergeplans) : NULL;
	if (IsA(plan, CustomScan))
		return ((CustomScan *) plan)->custom_plans != NIL ?
			linitial(((CustomScan *) plan)->custom_plans) : NULL;
	return plan->lefttree;
}

/* Whether a target list holds a Const or a Param, which setrefs keeps. */
static bool
constant_targets(List *exprs)
{
	foreach_ptr(Node, expr, exprs)
	{
		if (IsA(expr, Const) || IsA(expr, Param))
			return true;
	}
	return false;
}

bool
tess_path_setop_constant(const Path *path)
{
	if (path == NULL)
		return false;
	if (path->pathtarget == NULL ||
		!tess_plan_has_setop_columns((Node *) path->pathtarget->exprs))
	{
		/* A branch: its targets are the set operation's columns. */
		if (IsA(path, SubqueryScanPath) &&
			constant_targets(((const SubqueryScanPath *) path)->subpath->pathtarget->exprs))
			return true;
		return path->pathtarget != NULL && constant_targets(path->pathtarget->exprs);
	}
	switch (nodeTag(path))
	{
		case T_AppendPath:
			foreach_ptr(Path, child, ((const AppendPath *) path)->subpaths)
			{
				if (tess_path_setop_constant(child))
					return true;
			}
			return false;
		case T_MergeAppendPath:
			foreach_ptr(Path, child, ((const MergeAppendPath *) path)->subpaths)
			{
				if (tess_path_setop_constant(child))
					return true;
			}
			return false;
		case T_SetOpPath:
			return tess_path_setop_constant(((const SetOpPath *) path)->leftpath) ||
				tess_path_setop_constant(((const SetOpPath *) path)->rightpath);
		case T_CustomPath:
			foreach_ptr(Path, child, ((const CustomPath *) path)->custom_paths)
			{
				if (tess_path_setop_constant(child))
					return true;
			}
			return false;
		case T_AggPath:
			return tess_path_setop_constant(((const AggPath *) path)->subpath);
		case T_SortPath:
		case T_IncrementalSortPath:
			return tess_path_setop_constant(((const SortPath *) path)->subpath);
		case T_UniquePath:
			return tess_path_setop_constant(((const UniquePath *) path)->subpath);
		case T_GatherPath:
			return tess_path_setop_constant(((const GatherPath *) path)->subpath);
		case T_GatherMergePath:
			return tess_path_setop_constant(((const GatherMergePath *) path)->subpath);
		case T_LimitPath:
			return tess_path_setop_constant(((const LimitPath *) path)->subpath);
		case T_ProjectionPath:
			return tess_path_setop_constant(((const ProjectionPath *) path)->subpath);
		default:
			/* A path of the columns whose branches are not known here. */
			return true;
	}
}

Node *
tess_plan_setop_columns(Node *node, const Plan *child)
{
	if (child == NULL || !tess_plan_has_setop_columns(node))
		return node;
	while (tess_plan_has_setop_columns((Node *) child->targetlist))
	{
		child = first_child(child);
		if (child == NULL)
			elog(ERROR, "Tessera found no plan with targets for a set operation's columns");
	}
	return setop_columns_mutator(node, (void *) child->targetlist);
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
		config->layout_policy > TESS_LAYOUT_PROJECTED)
		elog(ERROR, "Tessera plan received an invalid layout policy");
	if ((config->layout_policy == TESS_LAYOUT_EXPLICIT ||
		 config->layout_policy == TESS_LAYOUT_PROJECTED) &&
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

TargetEntry *
tess_plan_child_entry(const TessPlanChild *child, Expr *expr)
{
	TargetEntry *entry;

	if (child == NULL || child->node == NULL || expr == NULL)
		return NULL;
	entry = tlist_member(expr, child->plan->targetlist);
	if (entry == NULL || tess_layout_column(&child->layout, entry->resno - 1) < 0)
		return NULL;
	return entry;
}

int
tess_plan_child_column(const TessPlanChild *child, Expr *expr)
{
	TargetEntry *entry = tess_plan_child_entry(child, expr);

	return entry == NULL ? -1 : tess_layout_column(&child->layout, entry->resno - 1);
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
	const List *qual = config->qual;
	const List *expressions = config->expressions;
	const List *given_scan_targetlist = config->scan_targetlist;

	check_plan_config(config);
	check_children(path, child_plans);
	tess_path_get_info(path, &path_info);
	/* Over a set operation's rows: its columns are the first child's targets. */
	if (child_plans != NIL)
	{
		const Plan *first = linitial(child_plans);

		targetlist = (List *) tess_plan_setop_columns((Node *) targetlist, first);
		qual = (List *) tess_plan_setop_columns((Node *) config->qual, first);
		expressions = (List *) tess_plan_setop_columns((Node *) config->expressions, first);
		given_scan_targetlist = (List *) tess_plan_setop_columns((Node *) config->scan_targetlist,
																	first);
	}
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
		case TESS_LAYOUT_PROJECTED:
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
	if (TESS_ABI_HAS_FIELD(config, TessPlanConfig, scan_tuple_is_relation) &&
		config->scan_tuple_is_relation)
	{
		/* The relation's row is the scan tuple: no custom_scan_tlist. */
		if (config->scanrelid == 0 || config->scan_targetlist != NIL ||
			config->layout_policy != TESS_LAYOUT_PROJECTED)
			elog(ERROR, "Tessera plan can use the relation as its scan tuple only when projected over a relation");
		scan_targetlist = NIL;
	}
	else if (given_scan_targetlist != NIL)
		scan_targetlist = copyObject(given_scan_targetlist);
	else if (config->layout_policy == TESS_LAYOUT_PRESERVE_CHILD)
		scan_targetlist = copyObject(((Plan *)
			list_nth(child_plans, config->layout_child))->targetlist);
	else
		scan_targetlist = copyObject(targetlist);
	/* A projected plan's targets are derived when it is read: the layout
	 * stored is its scan tuple's, one target per scan tuple entry. */
	if (config->layout_policy == TESS_LAYOUT_PROJECTED)
	{
		if (scan_targetlist != NIL &&
			layout.ntargets != list_length(scan_targetlist))
			elog(ERROR, "Tessera scan tuple layout does not match its scan target list");
	}
	else if (layout.ntargets != list_length(targetlist))
		elog(ERROR, "Tessera output layout does not match its target list");
	for (int target = 0; layout.target_columns != NULL &&
		 target < layout.ntargets; target++)
		target_columns = lappend_int(target_columns,
									 layout.target_columns[target]);

	scan = makeNode(CustomScan);
	scan->methods = config->methods;
	scan->flags = path->flags;
	scan->scan.scanrelid = config->scanrelid;
	scan->scan.plan.targetlist = copyObject(targetlist);
	scan->scan.plan.qual = copyObject(qual);
	scan->custom_plans = child_plans;
	scan->custom_exprs = copyObject(expressions);
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

/*
 * The batch column a target of a projected plan reads from the scan
 * tuple, or -1 for a target the node computes. With a scan target list,
 * the target is one of its entries, by position (INDEX_VAR after setrefs)
 * or by equality before; without one, the scan tuple is the relation's
 * row and the target is a column of it.
 */
static int
scan_tuple_column(const CustomScan *scan, const TessLayout *tuple, Node *expr)
{
	int			entry = -1;

	if (scan->custom_scan_tlist != NIL)
	{
		TargetEntry *found;

		if (IsA(expr, Var) && ((Var *) expr)->varno == INDEX_VAR)
			entry = ((Var *) expr)->varattno - 1;
		else if ((found = tlist_member((Expr *) expr, scan->custom_scan_tlist)) != NULL)
			entry = found->resno - 1;
	}
	else if (IsA(expr, Var) && ((Var *) expr)->varno == scan->scan.scanrelid)
		entry = ((Var *) expr)->varattno - 1;
	if (entry < 0 || entry >= tuple->ntargets)
		return -1;
	return tess_layout_column(tuple, entry);
}

/*
 * The final layout of a projected plan from its final target list: the
 * stored layout describes the scan tuple; a target that is a column of it
 * maps there, the others become computed columns after the scan tuple's.
 */
static void
derive_projected_layout(const CustomScan *scan, TessPlanInfo *result)
{
	TessLayout	tuple = result->layout;
	List	   *targets = scan->scan.plan.targetlist;
	int			ntargets = list_length(targets);
	int		   *map = ntargets > 0 ? palloc_array(int, ntargets) : NULL;
	int			ncomputed = 0;
	int			target = 0;

	foreach_ptr(TargetEntry, entry, targets)
	{
		int			column = scan_tuple_column(scan, &tuple, (Node *) entry->expr);

		if (column < 0)
		{
			column = tuple.ncolumns + ncomputed++;
			if (TESS_ABI_HAS_FIELD(result, TessPlanInfo, computed))
				result->computed = lappend(result->computed, entry);
		}
		map[target++] = column;
	}
	if (tuple.target_columns != NULL)
		pfree((void *) tuple.target_columns);
	result->layout.ncolumns = tuple.ncolumns + ncomputed;
	result->layout.ntargets = ntargets;
	result->layout.target_columns = map;
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
		layout_policy > TESS_LAYOUT_PROJECTED)
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
	if (TESS_ABI_HAS_FIELD(result, TessPlanInfo, computed))
		result->computed = NIL;
	if (layout_policy == TESS_LAYOUT_DENSE)
	{
		/* PostgreSQL may replace a projection after PlanCustomPath. */
		if (result->layout.target_columns != NULL)
			pfree((void *) result->layout.target_columns);
		result->layout.ncolumns = list_length(scan->scan.plan.targetlist);
		result->layout.ntargets = result->layout.ncolumns;
		result->layout.target_columns = NULL;
	}
	else if (layout_policy == TESS_LAYOUT_PROJECTED)
		derive_projected_layout(scan, result);
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

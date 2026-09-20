/* Batch CustomPaths and CustomScans built through the runtime library. */
#ifndef TESSERA_PLANNER_H
#define TESSERA_PLANNER_H

#include "postgres.h"

#include "nodes/extensible.h"
#include "nodes/pathnodes.h"
#include "nodes/plannodes.h"

#include "tessera/abi.h"
#include "tessera/layout.h"
#include "tessera/node.h"

/*
 * A node module owns its planner hooks, path selection and costs; the
 * helpers here only build the CustomPath and CustomScan the same way for
 * every batch node, so that other nodes and the executor helpers can
 * recognize a batch path and read its layout. See docs/runtime.md.
 */

/* Complete description of a CustomPath built from an existing path. */
typedef struct TessPathConfig
{
	Size		struct_size;
	/* Rows, costs, path keys and parallel properties are copied from it. */
	const Path *template_path;
	const CustomPathMethods *methods;
	/* The registered kind of node that owns the path. */
	const TessNode *node;
	/* Child paths; a child of another batch node stays recognizable. */
	List	   *children;
	List	   *restrictinfo;
	/* Carried to PlanCustomPath in copyObject-safe form. */
	const List *expressions;
	const Node *node_data;
	/* CUSTOMPATH_* flags. */
	uint32		flags;
} TessPathConfig;

#define TESS_PATH_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessPathConfig, flags)

/*
 * Build the path. The template must not be parameterized, and the node
 * must be registered under its name; both are errors otherwise.
 */
extern CustomPath *tess_path_create(const TessPathConfig *config);

/* True when the path is a CustomPath with these methods. */
extern bool tess_path_matches(const Path *path,
							  const CustomPathMethods *methods);

/*
 * The registered node that owns a path built here, or NULL for any other
 * path, including a CustomPath of another provider.
 */
extern const TessNode *tess_path_node(const Path *path);

/* What a path built here carries. */
typedef struct TessPathInfo
{
	Size		struct_size;
	const TessNode *node;
	List	   *expressions;
	Node	   *node_data;
} TessPathInfo;

#define TESS_PATH_INFO_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessPathInfo, node_data)

/* Read the path's data; the pointers are borrowed from the path. */
extern void tess_path_get_info(const CustomPath *path, TessPathInfo *result);

/* The registered node kind that turns rows into batches. */
#define TESS_PACK_NODE_NAME "tessera.pack"

/* The registered node kind that reads a heap relation in batches. */
#define TESS_HEAP_SCAN_NODE_NAME "tessera.heap_scan"

/*
 * A batch path over any path: the path itself when it is one, a batch scan
 * of its relation when the path is a sequential scan of a relation without
 * clauses and a node kind reads batches natively, otherwise the pack node's
 * path over it, so that a batch parent can stand above any core path. NULL
 * when the path is parameterized, when a pseudoconstant clause makes the
 * planner gate every scan of the relation with a Result the parent could
 * not read through, or when no pack node is registered; the caller then
 * adds no path.
 */
extern Path *tess_batch_input_path(PlannerInfo *root, Path *path);

/*
 * A batch scan of the relation a sequential scan path reads, evaluating
 * none of the relation's clauses, for a parent that evaluates them itself.
 * NULL when the path is not such a scan, is parameterized, is gated as
 * above, or no node kind takes it.
 */
extern Path *tess_batch_scan_path(PlannerInfo *root, Path *path);

/* How a plan describes the columns of the batches it publishes. */
typedef enum TessLayoutPolicy
{
	/*
	 * One column per entry of the final target list, including a projection
	 * that PostgreSQL installs after PlanCustomPath returns.
	 */
	TESS_LAYOUT_DENSE,
	/* The layout the node supplies. */
	TESS_LAYOUT_EXPLICIT,
	/* Keep one batch child's layout: a pass-through with the same targets. */
	TESS_LAYOUT_PRESERVE_CHILD
} TessLayoutPolicy;

/* Complete description of the CustomScan returned by PlanCustomPath. */
typedef struct TessPlanConfig
{
	Size		struct_size;
	const CustomScanMethods *methods;
	TessLayoutPolicy layout_policy;
	/* Copied, for TESS_LAYOUT_EXPLICIT. */
	const TessLayout *explicit_layout;
	/* Exactly what the executor evaluates; nothing is added implicitly. */
	const List *qual;
	const List *expressions;
	const Node *node_data;
	/*
	 * custom_scan_tlist, what the scan tuple contains. NULL takes the
	 * preserved child's target list, or otherwise the plan's own.
	 */
	const List *scan_targetlist;
	Index		scanrelid;
	/* Which child to preserve, for TESS_LAYOUT_PRESERVE_CHILD. */
	int			layout_child;
} TessPlanConfig;

#define TESS_PLAN_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessPlanConfig, layout_child)

/*
 * Build the plan of a path built here, in PlanCustomPath. Costs and relids
 * are copied from the path by PostgreSQL afterwards.
 */
extern Plan *tess_plan_create(CustomPath *path, List *targetlist,
							  List *child_plans, const TessPlanConfig *config);

/* One child seen while PlanCustomPath builds the plan. */
typedef struct TessPlanChild
{
	Size		struct_size;
	Path	   *path;
	Plan	   *plan;
	/* NULL when the child produces ordinary rows. */
	const TessNode *node;
	/* Valid when node is not NULL; the map is allocated for the caller. */
	TessLayout	layout;
} TessPlanChild;

#define TESS_PLAN_CHILD_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessPlanChild, layout)

/* Describe one child; true when it is a batch node. */
extern bool tess_plan_child(const CustomPath *path, const List *child_plans,
							int index, TessPlanChild *result);

/* What a plan built here carries, read in BeginCustomScan. */
typedef struct TessPlanInfo
{
	Size		struct_size;
	const TessNode *node;
	int			nchildren;
	/* A NULL entry is a child that produces ordinary rows. */
	const char **child_names;
	/* Derived from the final target list for TESS_LAYOUT_DENSE. */
	TessLayout	layout;
	Node	   *node_data;
} TessPlanInfo;

#define TESS_PLAN_INFO_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessPlanInfo, node_data)

/* The names and the layout's map are allocated for the caller. */
extern void tess_plan_get_info(const CustomScan *scan, TessPlanInfo *result);

/* The layout of a plan built here; the map is allocated for the caller. */
extern void tess_plan_get_layout(const Plan *plan, TessLayout *result);

#endif							/* TESSERA_PLANNER_H */

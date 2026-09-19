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

#endif							/* TESSERA_PLANNER_H */

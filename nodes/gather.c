#include "postgres.h"

#include "access/htup_details.h"
#include "access/parallel.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/execParallel.h"
#include "executor/executor.h"
#include "lib/binaryheap.h"
#include "miscadmin.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/planner.h"
#include "pgstat.h"
#include "storage/dsm.h"
#include "storage/latch.h"
#include "storage/proc.h"
#include "storage/shm_mq.h"
#include "utils/datum.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/wait_event.h"

#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessGather stands in for the core's Gather over a batch subtree, and
 * TessSend is the subtree's top in every worker (docs/nodes.md). The
 * core's Gather passes rows one by one: a worker forms a minimal tuple of
 * each and puts it into a queue, the leader reads and deforms it; through
 * that 1.33 M rows of a filtered scan took 27 to 30 ms with two workers
 * against 12.4 ms in one process. Here a worker sends batches: TessSend
 * copies the selected rows of its child's batches into messages of up to
 * GATHER_MESSAGE_ROWS rows, a lane of their NULL bits per 64 columns, a lane of words per
 * column and the bytes of the by-reference values, each column's word its
 * value or the value's byte in the message, and sends each whole through
 * a queue of its own in the node's chunk of the query's shared memory. The
 * leader's TessGather launches the workers as the core's Gather does, and
 * gives its parent the messages as batches of up to 64 rows whose columns
 * point into the message; while every queue is empty and the leader takes
 * part, it runs the subtree itself, reading TessSend's child directly.
 *
 * TessGatherMerge stands in for the core's Gather Merge the same way: each
 * worker's rows come in order, and its messages carry, after the columns'
 * lanes, a lane per word of the rows' sort items (tessera/sort.h), which
 * the leader merges with the kernel tess_sort_merge, its own rows among
 * them, as the last merge of an external TessSort does. A key of another
 * type ends the keys the words hold: its word is its abbreviated key, and
 * the leader merges in C, by the words and then the types' comparisons of
 * that key and the ones after it (TessSort's "Other types").
 */

/* Rows of a batch given out, and the most rows of a message. */
#define GATHER_ROWS 64
#define GATHER_MESSAGE_ROWS 1024
/* A message's lanes of NULL bits: column c takes bit c % 64 of lane c / 64. */
#define GATHER_NULL_LANES(ncolumns) (((ncolumns) + 63) / 64)
#define GATHER_MAX_NULL_LANES GATHER_NULL_LANES(MaxTupleAttributeNumber)
/* A queue per worker, as large as four of the core's tuple queues. */
#define GATHER_QUEUE_SIZE (256 * 1024)

/*
 * A row costs through TessGather, where the node's own paths are offered,
 * a share of parallel_tuple_cost, tessera.gather_tuple_share (0.25): 13.3 M
 * rows took the leader at most 3.5 ns each against 14 through the core's
 * Gather over the same nodes.
 */

/* What a message starts with; its lanes and values follow, aligned to 8. */
typedef struct GatherHeader
{
	uint32		nrows;
	uint32		ncolumns;
	/* The rows each lane has room for: the lanes' stride. */
	uint32		stride;
	/* The lanes of the rows' sort items' words, after the columns'. */
	uint32		key_words;
	uint64		values_len;
} GatherHeader;

/* The part of TessSend's chunk before the queues. */
typedef struct SendShared
{
	dsm_handle	segment;
	int			nqueues;
	/* The rows a parent needs, -1 for all: no worker sends more. */
	int64		bound;
} SendShared;

#define SEND_QUEUES_OFFSET MAXALIGN(sizeof(SendShared))

/*
 * A message being filled from a batch input: its lanes after the header,
 * its rows, the by-reference values' bytes; the batch being copied, the
 * last of its rows copied and how many, and its selected rows' key words.
 */
typedef struct MessageBuilder
{
	char	   *message;
	Size		message_len;
	uint32		rows;
	char	   *values;
	Size		values_len;
	Size		values_used;
	TessBatch  *batch;
	TessDatumColumn *columns;
	int			row;
	int			selected;
	uint64	   *key_lanes;
	int			key_capacity;
	/* A generic key's words of the batch. */
	Datum	   *abbrev_values;
	bool	   *abbrev_isnull;
	bool		exhausted;
} MessageBuilder;

typedef struct TessSendState
{
	CustomScanState css;
	PlanState  *child;
	/* The child read in a worker, which sends what it reads. */
	TessInput  *input;
	TessLayout	child_layout;
	int			ncolumns;
	int16	   *typlens;
	bool	   *typbyvals;
	/*
	 * Under TessGatherMerge, the keys its rows come in the order of: each
	 * one's target, how it orders, and the words of the rows' items each
	 * message carries in lanes.
	 */
	int			nkeys;
	int			key_places[TESS_TABLE_MAX_KEYS];
	TessSortKey keys[TESS_TABLE_MAX_KEYS];
	int			key_words;
	const TessKernelOps *kernels;
	/*
	 * A key of another type: the first one (-1 for none), the keys the
	 * words hold, up to it, its abbreviated keys in a context reset per
	 * batch, and the comparisons of it and the keys after it.
	 */
	int			generic;
	int			nkernel;
	TessSortAbbrev abbrev;
	MemoryContext abbrev_context;
	SortSupportData *ssup;
	/* The lanes of a message: NULL bits, the columns, the keys' words. */
	int			null_lanes;
	int			nlanes;
	/* The rows the parent above needs, -1 for all. */
	int64		bound;
	/* The rows of a message: a quarter of the queue for its lanes, 64 to GATHER_MESSAGE_ROWS. */
	uint32		stride;
	/* The leader's view of the queues, once laid out. */
	SendShared *shared;
	char	   *queues;
	/* A worker's queue and the message it fills. */
	shm_mq_handle *queue;
	MessageBuilder builder;
	uint64		sent_messages;
	uint64		sent_rows;
	bool		done;
} TessSendState;

/*
 * A stream TessGatherMerge merges: a worker's message in hand, or the
 * leader's own batch, its rows' places and key lanes; its rows and the next
 * one; done once it has no more.
 */
typedef struct MergeSource
{
	char	   *message;
	const char *values;
	uint32		stride;
	uint32		rows;
	uint32		place;
	bool		done;
} MergeSource;

typedef struct TessGatherState
{
	CustomScanState css;
	TessSendState  *send;
	bool		merge;
	int			num_workers;
	int64		bound;
	TessOutput *output;
	int			ncolumns;
	bool	   *typbyvals;
	/* The workers, once launched, and a queue handle each, NULL once it detached. */
	bool		initialized;
	ParallelExecutorInfo *pei;
	int			nworkers_launched;
	shm_mq_handle **readers;
	int			nreaders;
	int			nextreader;
	/* The leader's own part: TessSend's child read directly. */
	bool		need_local;
	bool		local_done;
	TessInput  *local;
	TessLayout	local_layout;
	TessBatch  *local_batch;
	/* The message being given out, its lanes' stride and the next of its rows. */
	char	   *message;
	uint32		message_rows;
	uint32		message_stride;
	uint32		next_row;
	/* The batch given out: a window of the message, or the leader's own batch. */
	TessBatch	batch;
	bool		published;
	bool		from_local;
	uint32		window_start;
	uint64		window_bits[1];
	Datum	  **values;
	bool	  **isnull;
	int			served;
	uint64		messages;
	uint64		worker_rows;
	uint64		local_rows;
	/*
	 * TessGatherMerge: a source per worker and the leader's last, whose
	 * rows it copies into messages of its own as a worker does; the
	 * merge's tree, kept between calls.
	 */
	MergeSource *sources;
	int			nsources;
	uint32		merge_state[TESS_SORT_MERGE_STATE_WORDS];
	MessageBuilder local_builder;
	/* A generic key's merge: the sources with rows in hand, by their next rows. */
	binaryheap *merge_heap;
} TessGatherState;

static const CustomExecMethods send_exec_methods;
static const CustomExecMethods gather_exec_methods;
static create_upper_paths_hook_type previous_create_upper_paths_hook = NULL;
static planner_hook_type previous_planner_hook = NULL;

/*
 * The parallel-aware plan nodes under a TessGather of the planning under
 * way, their flag held back until it is done: finalize_plan requires one
 * of the core's Gathers above every parallel-aware node once a query has
 * executor parameters, and knows no custom scan that gathers. The flag
 * gets a node its shared-memory callbacks; the rescan parameter a core
 * Gather adds for its nodes the node does without, rescanning its child
 * and reinitializing the shared memory itself.
 */
static List *held_parallel_aware = NIL;

static Plan *gather_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
						 List *tlist, List *clauses, List *custom_plans);
static Plan *gather_merge_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
							   List *tlist, List *clauses, List *custom_plans);
static Plan *send_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
					   List *tlist, List *clauses, List *custom_plans);

static const CustomPathMethods gather_path_methods = {
	.CustomName = "TessGather",
	.PlanCustomPath = gather_plan,
};

static const CustomPathMethods gather_merge_path_methods = {
	.CustomName = "TessGatherMerge",
	.PlanCustomPath = gather_merge_plan,
};

static const CustomPathMethods send_path_methods = {
	.CustomName = "TessSend",
	.PlanCustomPath = send_plan,
};

/* ---------------------------------------------------------------- planning */

/*
 * The batch path a gather of subpath reads: subpath itself, or, under a
 * projection the planner put there for a batch node that projects, a copy
 * of that node taking the projection's target, as the core's plan would
 * give it the projection. NULL where the core's gather stays: no worker,
 * or no batch path the workers may run of a column at least.
 */
static Path *
gathered_batch_path(Path *subpath, int num_workers)
{
	if (IsA(subpath, ProjectionPath) && ((ProjectionPath *) subpath)->dummypp)
	{
		ProjectionPath *projection = (ProjectionPath *) subpath;
		Path	   *below = projection->subpath;
		CustomPath *copy;

		if (tess_path_node(below) == NULL || !IsA(below, CustomPath) ||
			(((CustomPath *) below)->flags & CUSTOMPATH_SUPPORT_PROJECTION) == 0)
			return NULL;
		copy = makeNode(CustomPath);
		*copy = *(CustomPath *) below;
		copy->path.pathtarget = projection->path.pathtarget;
		subpath = &copy->path;
	}
	if (num_workers <= 0 || tess_path_node(subpath) == NULL || !subpath->parallel_safe ||
		subpath->param_info != NULL ||
		list_length(subpath->pathtarget->exprs) == 0 ||
		list_length(subpath->pathtarget->exprs) > MaxTupleAttributeNumber)
		return NULL;
	return subpath;
}

/*
 * TessSend over subpath and the gathering node over it, with the core
 * node's rows, costs and path keys; send_data the keys TessSend's messages
 * carry, if any. A gather that projects, of a target the workers may not
 * compute, becomes a projection over the node, which emits subpath's.
 */
static Path *
make_send_and_gather(PlannerInfo *root, Path *gather, Path *subpath, int num_workers,
					 List *send_data, const CustomPathMethods *methods, const TessNode *node)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	CustomPath *send;
	CustomPath *path;

	config.template_path = subpath;
	config.methods = &send_path_methods;
	config.node = &tess_send_node;
	config.children = list_make1(subpath);
	config.node_data = (Node *) send_data;
	send = tess_path_create(&config);
	/* Parallel-aware for the chunk of shared memory its queues take. */
	send->path.parallel_aware = true;
	config = (TessPathConfig) TESS_STRUCT_INITIALIZER(TessPathConfig);
	config.template_path = gather;
	config.methods = methods;
	config.node = node;
	config.children = list_make1(&send->path);
	config.node_data = (Node *) list_make1(makeInteger(num_workers));
	path = tess_path_create(&config);
	if (equal(gather->pathtarget->exprs, subpath->pathtarget->exprs))
		return &path->path;
	path->path.pathtarget = subpath->pathtarget;
	return (Path *) create_projection_path(root, gather->parent, &path->path, gather->pathtarget);
}

static Path *
make_gather_path(PlannerInfo *root, GatherPath *gather)
{
	Path	   *subpath = gathered_batch_path(gather->subpath, gather->num_workers);

	/* A single copy runs in one worker without the leader: only the core's plans make it. */
	if (gather->single_copy || subpath == NULL)
		return NULL;
	return make_send_and_gather(root, &gather->path, subpath, gather->num_workers, NIL,
								&gather_path_methods, &tess_gather_node);
}

/*
 * The node's path in place of a Gather Merge over a batch path whose path
 * keys the sort kernels order by: TessGatherMerge over TessSend, which
 * sends each row's key words with it. NULL where the core's stays.
 */
static Path *
make_gather_merge_path(PlannerInfo *root, GatherMergePath *gather)
{
	const TessKernelOps *kernels = tess_runtime_kernels();
	Path	   *subpath = gathered_batch_path(gather->subpath, gather->num_workers);
	List	   *places = NIL;
	List	   *kinds = NIL;
	List	   *flags = NIL;
	List	   *sortops = NIL;
	List	   *collations = NIL;
	int			nkeys = list_length(gather->path.pathkeys);

	if (subpath == NULL ||
		nkeys == 0 || nkeys > TESS_TABLE_MAX_KEYS || kernels == NULL ||
		!TESS_ABI_HAS_FIELD(kernels, TessKernelOps, sort_key_lanes))
		return NULL;
	foreach_node(PathKey, pathkey, gather->path.pathkeys)
	{
		TessSortKey key;
		int			place;
		Oid			sortop = InvalidOid;
		Oid			collation = InvalidOid;

		/* As TessSort takes them: a key of another type by its comparison. */
		if (tess_sort_key_of(pathkey, subpath->pathtarget, subpath->parent->relids,
							 &place, &key))
		{
			Oid			type = exprType(list_nth(subpath->pathtarget->exprs, place));

			sortop = get_opfamily_member_for_cmptype(pathkey->pk_opfamily, type, type,
													 pathkey->pk_cmptype);
			collation = pathkey->pk_eclass->ec_collation;
		}
		else if (tess_sort_generic_key(pathkey, subpath->pathtarget,
									   subpath->parent->relids, &place, &sortop, &collation))
		{
			key.kind = TESS_SORT_KIND_GENERIC;
			key.flags = (pathkey->pk_cmptype == COMPARE_GT ? TESS_SORT_DESCENDING : 0) |
				(pathkey->pk_nulls_first ? TESS_SORT_NULLS_FIRST : 0);
		}
		else
			return NULL;
		places = lappend_int(places, place);
		kinds = lappend_int(kinds, (int) key.kind);
		flags = lappend_int(flags, (int) key.flags);
		sortops = lappend_int(sortops, (int) sortop);
		collations = lappend_int(collations, (int) collation);
	}
	return make_send_and_gather(root, &gather->path, subpath, gather->num_workers,
								list_make5(places, kinds, flags, sortops, collations),
								&gather_merge_path_methods, &tess_gather_merge_node);
}

/*
 * A gather's cost at the node's share of parallel_tuple_cost for rows rows,
 * factor times each as the core's costs them.
 */
static void
discount_transfer(Path *path, double rows, double factor)
{
	path->total_cost -= (1.0 - tess_gather_tuple_share) * factor * parallel_tuple_cost * rows;
}

/*
 * TessGather over the cheapest partial path of rel, where that is a batch
 * path, at the node's cost of a row: the core gathers the same partial
 * path at its own cost after the hooks that call this, and add_path keeps
 * the cheaper. A base or join relation only, as the core's gathers; for
 * the topmost one the core applies the final target to it as to any path.
 */
void
tess_gather_add_paths(PlannerInfo *root, RelOptInfo *rel)
{
	Path	   *subpath;
	GatherPath *gather;
	Path	   *path;
	double		rows;

	if (!*tess_runtime_api()->settings->enable || !tess_batch_gather ||
		!rel->consider_parallel || rel->partial_pathlist == NIL ||
		(rel->reloptkind != RELOPT_BASEREL && rel->reloptkind != RELOPT_JOINREL))
		return;
	subpath = linitial(rel->partial_pathlist);
	if (tess_path_node(subpath) == NULL)
		return;
	rows = compute_gather_rows(subpath);
	gather = create_gather_path(root, rel, subpath, rel->reltarget, NULL, &rows);
	path = make_gather_path(root, gather);
	if (path == NULL)
		return;
	discount_transfer(path, gather->path.rows, 1.0);
	add_path(rel, path);
}

/*
 * TessGather over subpath, a partial batch path of rel, at the node's cost
 * of a row, with subpath's target: for a batch parent above, which the
 * core's gather would give rows. NULL where the node cannot gather it.
 */
Path *
tess_gather_path(PlannerInfo *root, RelOptInfo *rel, Path *subpath)
{
	GatherPath *gather;
	Path	   *path;
	double		rows;

	if (!*tess_runtime_api()->settings->enable || !tess_batch_gather)
		return NULL;
	rows = compute_gather_rows(subpath);
	gather = create_gather_path(root, rel, subpath, subpath->pathtarget, NULL, &rows);
	path = make_gather_path(root, gather);
	if (path == NULL)
		return NULL;
	discount_transfer(path, gather->path.rows, 1.0);
	return path;
}

/*
 * TessGatherMerge over sorted, a partial path of the node's sort in the
 * ordered relation, at the node's cost of a row, projected to target when
 * that differs; NULL where the node cannot merge it.
 */
Path *
tess_gather_merge_path(PlannerInfo *root, RelOptInfo *rel, Path *sorted, PathTarget *target)
{
	GatherMergePath *gather;
	Path	   *path;
	double		rows;

	if (!*tess_runtime_api()->settings->enable || !tess_batch_gather)
		return NULL;
	rows = compute_gather_rows(sorted);
	gather = create_gather_merge_path(root, rel, sorted, sorted->pathtarget, sorted->pathkeys,
									  NULL, &rows);
	path = make_gather_merge_path(root, gather);
	if (path == NULL)
		return NULL;
	discount_transfer(path, gather->path.rows, 1.05);
	if (!equal(path->pathtarget->exprs, target->exprs))
		path = (Path *) create_projection_path(root, rel, path, target);
	return path;
}

/*
 * Replace, under path, every Gather over a batch path by the node's path,
 * through the kinds of path that hold others; a kind not known here is
 * left as it is, and so is what is below it.
 */
static Path *
replace_gathers(PlannerInfo *root, Path *path)
{
	if (path == NULL)
		return NULL;
	switch (nodeTag(path))
	{
		case T_GatherPath:
			{
				Path	   *replaced = make_gather_path(root, (GatherPath *) path);

				return replaced != NULL ? replaced : path;
			}
		case T_GatherMergePath:
			{
				Path	   *replaced = make_gather_merge_path(root, (GatherMergePath *) path);

				return replaced != NULL ? replaced : path;
			}
		case T_ProjectionPath:
			((ProjectionPath *) path)->subpath =
				replace_gathers(root, ((ProjectionPath *) path)->subpath);
			break;
		case T_ProjectSetPath:
			((ProjectSetPath *) path)->subpath =
				replace_gathers(root, ((ProjectSetPath *) path)->subpath);
			break;
		case T_SortPath:
			((SortPath *) path)->subpath =
				replace_gathers(root, ((SortPath *) path)->subpath);
			break;
		case T_IncrementalSortPath:
			((IncrementalSortPath *) path)->spath.subpath =
				replace_gathers(root, ((IncrementalSortPath *) path)->spath.subpath);
			break;
		case T_AggPath:
			((AggPath *) path)->subpath =
				replace_gathers(root, ((AggPath *) path)->subpath);
			break;
		case T_GroupPath:
			((GroupPath *) path)->subpath =
				replace_gathers(root, ((GroupPath *) path)->subpath);
			break;
		case T_UniquePath:
			((UniquePath *) path)->subpath =
				replace_gathers(root, ((UniquePath *) path)->subpath);
			break;
		case T_WindowAggPath:
			((WindowAggPath *) path)->subpath =
				replace_gathers(root, ((WindowAggPath *) path)->subpath);
			break;
		case T_LimitPath:
			((LimitPath *) path)->subpath =
				replace_gathers(root, ((LimitPath *) path)->subpath);
			break;
		case T_LockRowsPath:
			((LockRowsPath *) path)->subpath =
				replace_gathers(root, ((LockRowsPath *) path)->subpath);
			break;
		case T_MaterialPath:
			((MaterialPath *) path)->subpath =
				replace_gathers(root, ((MaterialPath *) path)->subpath);
			break;
		case T_MemoizePath:
			((MemoizePath *) path)->subpath =
				replace_gathers(root, ((MemoizePath *) path)->subpath);
			break;
		case T_NestPath:
		case T_MergePath:
		case T_HashPath:
			{
				JoinPath   *join = (JoinPath *) path;

				join->outerjoinpath = replace_gathers(root, join->outerjoinpath);
				join->innerjoinpath = replace_gathers(root, join->innerjoinpath);
				break;
			}
		case T_GroupingSetsPath:
			((GroupingSetsPath *) path)->subpath =
				replace_gathers(root, ((GroupingSetsPath *) path)->subpath);
			break;
		case T_ModifyTablePath:
			((ModifyTablePath *) path)->subpath =
				replace_gathers(root, ((ModifyTablePath *) path)->subpath);
			break;
		case T_SetOpPath:
			((SetOpPath *) path)->leftpath = replace_gathers(root, ((SetOpPath *) path)->leftpath);
			((SetOpPath *) path)->rightpath = replace_gathers(root, ((SetOpPath *) path)->rightpath);
			break;
		case T_RecursiveUnionPath:
			((RecursiveUnionPath *) path)->leftpath =
				replace_gathers(root, ((RecursiveUnionPath *) path)->leftpath);
			((RecursiveUnionPath *) path)->rightpath =
				replace_gathers(root, ((RecursiveUnionPath *) path)->rightpath);
			break;
		case T_MinMaxAggPath:
			/* Each aggregate's subquery is planned without the final stage's hook. */
			foreach_node(MinMaxAggInfo, info, ((MinMaxAggPath *) path)->mmaggregates)
				info->path = replace_gathers(info->subroot, info->path);
			break;
		case T_AppendPath:
			{
				ListCell   *lc;

				foreach(lc, ((AppendPath *) path)->subpaths)
					lfirst(lc) = replace_gathers(root, lfirst(lc));
				break;
			}
		case T_MergeAppendPath:
			{
				ListCell   *lc;

				foreach(lc, ((MergeAppendPath *) path)->subpaths)
					lfirst(lc) = replace_gathers(root, lfirst(lc));
				break;
			}
		case T_CustomPath:
			{
				CustomPath *custom = (CustomPath *) path;
				const TessNode *node = tess_path_node(path);
				ListCell   *lc;

				foreach(lc, custom->custom_paths)
					lfirst(lc) = replace_gathers(root, lfirst(lc));
				/*
				 * A pack over a Gather made batches of its rows: over the
				 * node's path, which gives batches, it goes.
				 */
				if (node != NULL && strcmp(node->name, TESS_PACK_NODE_NAME) == 0 &&
					list_length(custom->custom_paths) == 1 &&
					(tess_path_node(linitial(custom->custom_paths)) == &tess_gather_node ||
					 tess_path_node(linitial(custom->custom_paths)) == &tess_gather_merge_node) &&
					equal(path->pathtarget->exprs,
						  ((Path *) linitial(custom->custom_paths))->pathtarget->exprs))
					return linitial(custom->custom_paths);
				break;
			}
		default:
			break;
	}
	return path;
}

/*
 * Once the query's paths are final, every Gather over a batch path in
 * them gives way to the node's: the choice between plans stays the
 * core's, made at its costs.
 */
static void
create_upper_paths(PlannerInfo *root, UpperRelationKind stage,
				   RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra)
{
	ListCell   *lc;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel, extra);
	if (!*tess_runtime_api()->settings->enable || !tess_batch_gather ||
		stage != UPPERREL_FINAL)
		return;
	foreach(lc, output_rel->pathlist)
		lfirst(lc) = replace_gathers(root, lfirst(lc));
}

/* TessSend keeps its child's layout: it sends the child's targets in order. */
static Plan *
send_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path, List *tlist,
		  List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	List	   *data = NIL;
	TessPlanWriter *writer;

	tess_path_get_info(best_path, &info);
	data = (List *) info.node_data;
	/* The keys a merge above orders by, none under TessGather. */
	writer = tess_plan_writer_create(TESS_SEND_DATA, TESS_SEND_DATA_VERSION);
	tess_plan_write_int_list(writer, "keys", data == NIL ? NIL : linitial(data));
	tess_plan_write_int_list(writer, "kinds", data == NIL ? NIL : lsecond(data));
	tess_plan_write_int_list(writer, "flags", data == NIL ? NIL : lthird(data));
	tess_plan_write_int_list(writer, "sortops", data == NIL ? NIL : lfourth(data));
	tess_plan_write_int_list(writer, "collations", data == NIL ? NIL : list_nth(data, 4));
	config.methods = &tess_send_scan_methods;
	config.layout_policy = TESS_LAYOUT_PRESERVE_CHILD;
	config.layout_child = 0;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

/* Hold back the parallel-aware flag of plan and the nodes below it. */
static void
hold_parallel_aware(Plan *plan)
{
	ListCell   *lc;

	if (plan == NULL)
		return;
	check_stack_depth();
	if (plan->parallel_aware)
	{
		plan->parallel_aware = false;
		held_parallel_aware = lappend(held_parallel_aware, plan);
	}
	hold_parallel_aware(plan->lefttree);
	hold_parallel_aware(plan->righttree);
	switch (nodeTag(plan))
	{
		case T_CustomScan:
			foreach(lc, ((CustomScan *) plan)->custom_plans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_Append:
			foreach(lc, ((Append *) plan)->appendplans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_MergeAppend:
			foreach(lc, ((MergeAppend *) plan)->mergeplans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_BitmapAnd:
			foreach(lc, ((BitmapAnd *) plan)->bitmapplans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_BitmapOr:
			foreach(lc, ((BitmapOr *) plan)->bitmapplans)
				hold_parallel_aware(lfirst(lc));
			break;
		case T_SubqueryScan:
			hold_parallel_aware(((SubqueryScan *) plan)->subplan);
			break;
		default:
			break;
	}
}

/* The planning done, the flags held back come back; a nested planning keeps its own. */
static PlannedStmt *
gather_planner(Query *parse, const char *query_string, int cursorOptions,
			   ParamListInfo boundParams
#if PG_VERSION_NUM >= 190000
			   ,ExplainState *es
#endif
	)
{
	List	   *outer = held_parallel_aware;
	PlannedStmt *result;

	held_parallel_aware = NIL;
	PG_TRY();
	{
#if PG_VERSION_NUM >= 190000
		result = previous_planner_hook != NULL ?
			previous_planner_hook(parse, query_string, cursorOptions, boundParams, es) :
			standard_planner(parse, query_string, cursorOptions, boundParams, es);
#else
		result = previous_planner_hook != NULL ?
			previous_planner_hook(parse, query_string, cursorOptions, boundParams) :
			standard_planner(parse, query_string, cursorOptions, boundParams);
#endif
	}
	PG_CATCH();
	{
		held_parallel_aware = outer;
		PG_RE_THROW();
	}
	PG_END_TRY();
	foreach_ptr(Plan, plan, held_parallel_aware)
		plan->parallel_aware = true;
	list_free(held_parallel_aware);
	held_parallel_aware = outer;
	return result;
}

/* TessGather and TessGatherMerge: a column per target, the targets TessSend sends. */
static Plan *
gather_plan_of(PlannerInfo *root, CustomPath *best_path, List *tlist, List *custom_plans,
			   const CustomScanMethods *methods)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanWriter *writer;

	tess_path_get_info(best_path, &info);
	/* The plan launches workers, as the core's Gather's does. */
	root->glob->parallelModeNeeded = true;
	writer = tess_plan_writer_create(TESS_GATHER_DATA, TESS_GATHER_DATA_VERSION);
	tess_plan_write_int(writer, "workers", intVal(linitial((List *) info.node_data)));
	config.methods = methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	foreach_ptr(Plan, child, custom_plans)
		hold_parallel_aware(child);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

static Plan *
gather_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path, List *tlist,
			List *clauses, List *custom_plans)
{
	return gather_plan_of(root, best_path, tlist, custom_plans, &tess_gather_scan_methods);
}

static Plan *
gather_merge_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path, List *tlist,
				  List *clauses, List *custom_plans)
{
	return gather_plan_of(root, best_path, tlist, custom_plans, &tess_gather_merge_scan_methods);
}

/* ---------------------------------------------------------------- TessSend */

/*
 * The words of the items a merge compares, as a run of an external
 * TessSort keeps them: every key takes its bit for NULL, and the last word
 * goes when it holds no key's bits, the reference's only.
 */
static int
merge_key_words(const TessKernelOps *kernels, int nkeys, TessSortKey *keys)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
	int			words;
	int			bits = 0;

	for (int key = 0; key < nkeys; key++)
	{
		keys[key].flags |= TESS_SORT_NULLABLE;
		bits += (keys[key].kind == TESS_TABLE_KEY_INT8 ? 64 : 32) + 1;
	}
	tess_status_check(kernels->sort_item_words(nkeys, keys, &words, &status), &status);
	return bits <= 64 * (words - 1) ? words - 1 : words;
}

static Node *
send_create_state(CustomScan *cscan)
{
	TessSendState  *state = (TessSendState *) newNode(sizeof(TessSendState), T_CustomScanState);

	state->css.methods = &send_exec_methods;
	return (Node *) state;
}

static void
send_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessSendState  *state = (TessSendState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TupleDesc	desc = css->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor;
	TessPlanReader *reader;
	List	   *places;
	List	   *kinds;
	List	   *flags;
	List	   *sortops;
	List	   *collations;

	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_send_node || info.nchildren != 1 ||
		info.child_names[0] == NULL)
		elog(ERROR, "TessSend received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, TESS_SEND_DATA,
									 TESS_SEND_DATA_VERSION);
	places = tess_plan_read_int_list(reader, "keys");
	kinds = tess_plan_read_int_list(reader, "kinds");
	flags = tess_plan_read_int_list(reader, "flags");
	sortops = tess_plan_read_int_list(reader, "sortops");
	collations = tess_plan_read_int_list(reader, "collations");
	tess_plan_reader_finish(reader);
	state->bound = -1;
	state->child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->child_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	tess_plan_get_layout(state->child->plan, &state->child_layout);
	state->ncolumns = desc->natts;
	if (state->ncolumns != state->child_layout.ntargets || state->ncolumns == 0)
		elog(ERROR, "TessSend received a foreign plan");
	state->typlens = palloc_array(int16, Max(state->ncolumns, 1));
	state->typbyvals = palloc_array(bool, Max(state->ncolumns, 1));
	for (int column = 0; column < state->ncolumns; column++)
	{
		state->typlens[column] = TupleDescAttr(desc, column)->attlen;
		state->typbyvals[column] = TupleDescAttr(desc, column)->attbyval;
	}
	state->nkeys = list_length(places);
	if (state->nkeys > TESS_TABLE_MAX_KEYS || list_length(kinds) != state->nkeys ||
		list_length(flags) != state->nkeys || list_length(sortops) != state->nkeys ||
		list_length(collations) != state->nkeys)
		elog(ERROR, "TessSend received a foreign plan");
	state->generic = -1;
	for (int key = 0; key < state->nkeys; key++)
	{
		int			kind = list_nth_int(kinds, key);

		state->key_places[key] = list_nth_int(places, key);
		if (kind == TESS_SORT_KIND_GENERIC && state->generic < 0)
			state->generic = key;
		state->keys[key].kind = kind == TESS_SORT_KIND_GENERIC ? TESS_TABLE_KEY_INT8 :
			(TessTableKeyKind) kind;
		state->keys[key].flags = (uint32) list_nth_int(flags, key);
		if (state->key_places[key] < 0 || state->key_places[key] >= state->ncolumns)
			elog(ERROR, "TessSend received a foreign plan");
	}
	state->nkernel = state->generic < 0 ? state->nkeys : state->generic + 1;
	if (state->generic >= 0)
	{
		int			first = state->generic;

		state->ssup = palloc0_array(SortSupportData, state->nkeys);
		for (int key = first; key < state->nkeys; key++)
		{
			if (!OidIsValid((Oid) list_nth_int(sortops, key)))
				elog(ERROR, "TessSend received a foreign plan");
			tess_sort_support(&state->ssup[key], (Oid) list_nth_int(sortops, key),
							  (Oid) list_nth_int(collations, key),
							  (state->keys[key].flags & TESS_SORT_NULLS_FIRST) != 0);
		}
		tess_sort_abbrev_init(&state->abbrev, (Oid) list_nth_int(sortops, first),
							  state->ssup[first].ssup_collation,
							  state->ssup[first].ssup_nulls_first,
							  TupleDescAttr(desc, state->key_places[first])->atttypid);
		state->abbrev_context = AllocSetContextCreate(CurrentMemoryContext,
													  "TessSend abbreviated keys",
													  ALLOCSET_DEFAULT_SIZES);
	}
	if (state->nkeys > 0)
	{
		state->kernels = tess_runtime_kernels();
		if (state->kernels == NULL ||
			!TESS_ABI_HAS_FIELD(state->kernels, TessKernelOps, sort_key_lanes))
			elog(ERROR, "TessSend needs the Tessera kernels module");
		state->key_words = merge_key_words(state->kernels, state->nkernel, state->keys);
	}
	state->null_lanes = GATHER_NULL_LANES(state->ncolumns);
	state->nlanes = state->null_lanes + state->ncolumns + state->key_words;
	state->stride = (uint32) Min((Size) GATHER_MESSAGE_ROWS,
								 Max((Size) GATHER_ROWS,
									 GATHER_QUEUE_SIZE / 4 /
									 (sizeof(uint64) * state->nlanes)));
	/* A worker reads the child and sends; the leader's TessGather reads the child itself. */
	if (IsParallelWorker())
	{
		TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
		Bitmapset  *columns = NULL;

		state->input = tess_input_create(estate->es_query_cxt, state->child);
		for (int target = 0; target < state->ncolumns; target++)
			columns = bms_add_member(columns,
									 tess_layout_column(&state->child_layout, target));
		request.projection_columns = columns;
		request.output_mode = TESS_OUTPUT_BATCH;
		tess_input_set_request(state->input, &request);
	}
}

/* The bytes of a message of nlanes lanes of rows rows before its values. */
static Size
message_head(int nlanes, uint32 rows)
{
	return MAXALIGN(sizeof(GatherHeader)) + sizeof(uint64) * (Size) rows * nlanes;
}

/*
 * Copy the selected rows of the input's batches into the builder's
 * message, from the rows left of the batch it stopped in: true once the
 * message is full, its rows at the stride or its values past half a queue,
 * with more rows to come; false once the input is done.
 */
static bool
fill_message(TessSendState *send, MessageBuilder *builder, TessInput *input,
			 MemoryContext context)
{
	uint64	   *lanes;

	if (builder->message == NULL)
	{
		builder->message_len = message_head(send->nlanes, send->stride);
		builder->message = MemoryContextAllocZero(context, builder->message_len);
		builder->values_len = 64 * 1024;
		builder->values = MemoryContextAlloc(context, builder->values_len);
		builder->columns = MemoryContextAlloc(context,
											  sizeof(TessDatumColumn) * send->ncolumns);
	}
	lanes = (uint64 *) (builder->message + MAXALIGN(sizeof(GatherHeader)));
	builder->rows = 0;
	builder->values_used = 0;
	if (builder->exhausted)
		return false;
	for (;;)
	{
		TessBatch  *batch = builder->batch;
		TessDatumColumn *columns = builder->columns;
		int			row;

		if (batch == NULL)
		{
			batch = tess_input_next(input);
			if (batch == NULL)
			{
				builder->exhausted = true;
				return false;
			}
			for (int column = 0; column < send->ncolumns; column++)
			{
				columns[column] = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
				batch->ops->get_datum_column(batch,
											 tess_layout_column(&send->child_layout, column),
											 &batch->rows, TESS_COLUMN_FOR_PROJECTION,
											 &columns[column]);
				if (columns[column].values == NULL || columns[column].isnull == NULL ||
					columns[column].nrows != batch->rows.nrows)
					elog(ERROR, "Tessera batch returned an invalid column");
			}
			/* The selected rows' key words, in order, for a merge above. */
			if (send->nkeys > 0)
			{
				TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
				TessTableKey table_keys[TESS_TABLE_MAX_KEYS];
				uint64	   *key_lanes[TESS_SORT_MAX_ITEM_WORDS];
				int			count;

				TessDatumColumn abbreviated = TESS_STRUCT_INITIALIZER(TessDatumColumn);

				if (batch->rows.nrows > builder->key_capacity)
				{
					if (builder->key_lanes != NULL)
						pfree(builder->key_lanes);
					if (builder->abbrev_values != NULL)
					{
						pfree(builder->abbrev_values);
						pfree(builder->abbrev_isnull);
					}
					builder->key_capacity = Max(batch->rows.nrows, GATHER_ROWS);
					builder->key_lanes = MemoryContextAlloc(context, sizeof(uint64) *
															send->key_words *
															builder->key_capacity);
					builder->abbrev_values = MemoryContextAllocZero(context, sizeof(Datum) *
																	builder->key_capacity);
					builder->abbrev_isnull = MemoryContextAllocZero(context, sizeof(bool) *
																	builder->key_capacity);
				}
				for (int key = 0; key < send->nkernel; key++)
				{
					table_keys[key].kind = send->keys[key].kind;
					table_keys[key].column = &columns[send->key_places[key]];
					table_keys[key].prepared = NULL;
				}
				/* A generic key's word: its abbreviated key. */
				if (send->generic >= 0)
				{
					const TessDatumColumn *column = &columns[send->key_places[send->generic]];
					MemoryContext old;

					MemoryContextReset(send->abbrev_context);
					old = MemoryContextSwitchTo(send->abbrev_context);
					for (int at = -1; (at = tess_row_mask_next(&batch->rows, at)) >= 0;)
					{
						builder->abbrev_isnull[at] = column->isnull[at];
						builder->abbrev_values[at] = Int64GetDatum(column->isnull[at] ? 0 :
																   tess_sort_abbrev_word(&send->abbrev,
																						 column->values[at]));
					}
					MemoryContextSwitchTo(old);
					abbreviated.values = builder->abbrev_values;
					abbreviated.isnull = builder->abbrev_isnull;
					abbreviated.nrows = batch->rows.nrows;
					table_keys[send->generic].column = &abbreviated;
				}
				for (int word = 0; word < send->key_words; word++)
					key_lanes[word] = builder->key_lanes + (Size) word * builder->key_capacity;
				tess_status_check(send->kernels->sort_key_lanes(send->nkernel, send->keys, table_keys,
																&batch->rows, send->key_words,
																key_lanes, builder->key_capacity,
																&count, &status),
								  &status);
			}
			builder->batch = batch;
			builder->row = -1;
			builder->selected = 0;
		}
		while ((row = tess_row_mask_next(&batch->rows, builder->row)) >= 0)
		{
			Size		need = 0;

			for (int column = 0; column < send->ncolumns; column++)
				if (!columns[column].isnull[row] && !send->typbyvals[column])
					need += MAXALIGN(datumGetSize(columns[column].values[row], false,
												  send->typlens[column]));
			if (builder->rows == send->stride ||
				(builder->rows > 0 && builder->values_used + need > GATHER_QUEUE_SIZE / 2))
				return true;
			if (builder->values_used + need > builder->values_len)
			{
				builder->values_len = Max(builder->values_len * 2, builder->values_used + need);
				builder->values = repalloc_huge(builder->values, builder->values_len);
			}
			for (int lane = 0; lane < send->null_lanes; lane++)
				lanes[(Size) send->stride * lane + builder->rows] = 0;
			for (int column = 0; column < send->ncolumns; column++)
			{
				uint64	   *lane = lanes + (Size) send->stride * (send->null_lanes + column);

				if (columns[column].isnull[row])
				{
					lanes[(Size) send->stride * (column / 64) + builder->rows] |=
						UINT64CONST(1) << (column % 64);
					lane[builder->rows] = 0;
				}
				else if (send->typbyvals[column])
					lane[builder->rows] = (uint64) columns[column].values[row];
				else
				{
					Size		size = datumGetSize(columns[column].values[row], false,
													send->typlens[column]);

					memcpy(builder->values + builder->values_used,
						   DatumGetPointer(columns[column].values[row]), size);
					lane[builder->rows] = builder->values_used;
					builder->values_used += MAXALIGN(size);
				}
			}
			for (int word = 0; word < send->key_words; word++)
				lanes[(Size) send->stride * (send->null_lanes + send->ncolumns + word) +
					  builder->rows] =
					builder->key_lanes[(Size) word * builder->key_capacity + builder->selected];
			builder->rows++;
			builder->row = row;
			builder->selected++;
		}
		tess_input_finish(input);
		builder->batch = NULL;
		CHECK_FOR_INTERRUPTS();
	}
}

/* Send the message the worker filled, whole; false once the leader left. */
static bool
send_message(TessSendState *state)
{
	MessageBuilder *builder = &state->builder;
	GatherHeader *header = (GatherHeader *) builder->message;
	shm_mq_iovec parts[2];
	shm_mq_result result;

	if (builder->rows == 0)
		return true;
	header->nrows = builder->rows;
	header->ncolumns = state->ncolumns;
	header->stride = state->stride;
	header->key_words = (uint32) state->key_words;
	header->values_len = builder->values_used;
	/* The lanes as filled, for stride rows each, then the values. */
	parts[0].data = builder->message;
	parts[0].len = builder->message_len;
	parts[1].data = builder->values;
	parts[1].len = builder->values_used;
	result = shm_mq_sendv(state->queue, parts, builder->values_used > 0 ? 2 : 1, false, true);
	state->sent_messages++;
	state->sent_rows += builder->rows;
	return result == SHM_MQ_SUCCESS;
}

/* A worker: every batch of the child into messages, sent as they fill. */
static TupleTableSlot *
send_exec(CustomScanState *css)
{
	TessSendState  *state = (TessSendState *) css;

	if (state->input == NULL)
		elog(ERROR, "TessSend runs only in a parallel worker");
	if (state->done)
		return NULL;
	if (state->queue == NULL)
		elog(ERROR, "TessSend has no queue");
	for (;;)
	{
		bool		full = fill_message(state, &state->builder, state->input,
										css->ss.ps.state->es_query_cxt);

		if (!send_message(state))
		{
			/* The leader left: a limit above was met. */
			if (state->builder.batch != NULL)
				tess_input_finish(state->input);
			state->builder.batch = NULL;
			state->done = true;
			return NULL;
		}
		if (!full)
			break;
	}
	state->done = true;
	shm_mq_detach(state->queue);
	state->queue = NULL;
	return NULL;
}

static void
send_end(CustomScanState *css)
{
	TessSendState  *state = (TessSendState *) css;

	if (state->queue != NULL)
		shm_mq_detach(state->queue);
	state->queue = NULL;
	ExecEndNode(state->child);
}

static void
send_rescan(CustomScanState *css)
{
	TessSendState  *state = (TessSendState *) css;

	ExecReScan(state->child);
	if (state->input != NULL)
		tess_input_rescan(state->input);
	state->builder.batch = NULL;
	state->builder.exhausted = false;
	state->done = false;
}

static Size
send_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	return add_size(SEND_QUEUES_OFFSET,
					mul_size(Max(pcxt->nworkers, 1), GATHER_QUEUE_SIZE));
}

/* The leader lays a queue out per worker, receiving on each itself. */
static void
send_lay_out(TessSendState *state, ParallelContext *pcxt, void *coordinate)
{
	state->shared = coordinate;
	state->shared->segment = dsm_segment_handle(pcxt->seg);
	state->shared->nqueues = pcxt->nworkers;
	state->shared->bound = state->bound;
	state->queues = (char *) coordinate + SEND_QUEUES_OFFSET;
	for (int worker = 0; worker < pcxt->nworkers; worker++)
	{
		shm_mq	   *queue = shm_mq_create(state->queues + (Size) worker * GATHER_QUEUE_SIZE,
										  GATHER_QUEUE_SIZE);

		shm_mq_set_receiver(queue, MyProc);
	}
}

static void
send_initialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	send_lay_out((TessSendState *) css, pcxt, coordinate);
}

static void
send_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	send_lay_out((TessSendState *) css, pcxt, coordinate);
}

/* A worker sends on its own queue. */
static void
send_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessSendState  *state = (TessSendState *) css;
	SendShared *shared = coordinate;
	dsm_segment *segment = dsm_find_mapping(shared->segment);
	shm_mq	   *queue;

	if (segment == NULL || ParallelWorkerNumber >= shared->nqueues)
		elog(ERROR, "TessSend found no queue of its own");
	queue = (shm_mq *) ((char *) coordinate + SEND_QUEUES_OFFSET +
						(Size) ParallelWorkerNumber * GATHER_QUEUE_SIZE);
	shm_mq_set_sender(queue, MyProc);
	state->queue = shm_mq_attach(queue, segment, NULL);
	/* A worker's share needs no more rows than the whole. */
	if (shared->bound >= 0)
		tess_set_child_bound(state->child, shared->bound);
}

static void
send_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
}

static const CustomExecMethods send_exec_methods = {
	.CustomName = "TessSend",
	.BeginCustomScan = send_begin,
	.ExecCustomScan = send_exec,
	.EndCustomScan = send_end,
	.ReScanCustomScan = send_rescan,
	.ExplainCustomScan = send_explain,
	.EstimateDSMCustomScan = send_estimate_dsm,
	.InitializeDSMCustomScan = send_initialize_dsm,
	.ReInitializeDSMCustomScan = send_reinitialize_dsm,
	.InitializeWorkerCustomScan = send_initialize_worker,
};

/* -------------------------------------------------------------- TessGather */

static Node *
gather_create_state(CustomScan *cscan)
{
	TessGatherState *state = (TessGatherState *) newNode(sizeof(TessGatherState), T_CustomScanState);

	state->css.methods = &gather_exec_methods;
	return (Node *) state;
}

/* A column of the window given out: pointers into the message, or the leader's batch's. */
static void
gather_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessGatherState *state = (TessGatherState *) batch->private_data;

	if (column < 0 || column >= state->ncolumns)
		elog(ERROR, "TessGather has no column %d", column);
	if (state->from_local)
	{
		TessBatch  *local = state->local_batch;

		local->ops->get_datum_column(local, tess_layout_column(&state->local_layout, column),
									 rows, purpose, result);
		return;
	}
	result->values = state->values[column];
	result->isnull = state->isnull[column];
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps gather_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = gather_get_column,
};

static void
gather_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessGatherState *state = (TessGatherState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TupleDesc	desc = css->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor;
	TessPlanReader *reader;
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	Bitmapset  *columns = NULL;
	PlanState  *send;

	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessGather supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if ((info.node != &tess_gather_node && info.node != &tess_gather_merge_node) ||
		info.nchildren != 1 || info.child_names[0] == NULL)
		elog(ERROR, "TessGather received a foreign plan");
	state->merge = info.node == &tess_gather_merge_node;
	state->bound = -1;
	reader = tess_plan_reader_create((List *) info.node_data, TESS_GATHER_DATA,
									 TESS_GATHER_DATA_VERSION);
	state->num_workers = tess_plan_read_int(reader, "workers");
	tess_plan_reader_finish(reader);
	send = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(send);
	if (!IsA(send, CustomScanState) ||
		((CustomScanState *) send)->methods != &send_exec_methods)
		elog(ERROR, "TessGather expected TessSend below it");
	state->send = (TessSendState *) send;
	state->ncolumns = desc->natts;
	if (state->ncolumns != state->send->ncolumns ||
		state->merge != (state->send->nkeys > 0))
		elog(ERROR, "TessGather received a foreign plan");
	state->typbyvals = palloc_array(bool, Max(state->ncolumns, 1));
	state->values = palloc_array(Datum *, Max(state->ncolumns, 1));
	state->isnull = palloc_array(bool *, Max(state->ncolumns, 1));
	for (int column = 0; column < state->ncolumns; column++)
	{
		state->typbyvals[column] = TupleDescAttr(desc, column)->attbyval;
		state->values[column] = palloc_array(Datum, GATHER_ROWS);
		state->isnull[column] = palloc_array(bool, GATHER_ROWS);
	}
	/* The leader's own part reads TessSend's child as a batch input. */
	state->local = tess_input_create(estate->es_query_cxt, state->send->child);
	state->local_layout = state->send->child_layout;
	for (int target = 0; target < state->ncolumns; target++)
		columns = bms_add_member(columns, tess_layout_column(&state->local_layout, target));
	request.projection_columns = columns;
	request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->local, &request);
	state->batch.abi_version = TESS_BATCH_ABI_VERSION;
	state->batch.struct_size = sizeof(TessBatch);
	state->batch.table_oid = InvalidOid;
	state->batch.ops = &gather_batch_ops;
	state->batch.private_data = state;
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot, &info.layout);
}

/* Launch the workers on the first execution, as ExecGather does. */
static void
gather_launch(TessGatherState *state)
{
	EState	   *estate = state->css.ss.ps.state;
	PlanState  *send = &state->send->css.ss.ps;

	state->nreaders = 0;
	if (state->num_workers > 0 && estate->es_use_parallel_mode)
	{
		ParallelContext *pcxt;
		Bitmapset  *params = bms_copy(send->plan->extParam);

		/* The initplans' values below go to the workers, as a Gather's initParam. */
		state->send->bound = state->bound;
		if (state->pei == NULL)
			state->pei = ExecInitParallelPlan(send, estate, params, state->num_workers,
											  state->bound);
		else
			ExecParallelReinitialize(send, state->pei, params);
		pcxt = state->pei->pcxt;
		LaunchParallelWorkers(pcxt);
		state->nworkers_launched = pcxt->nworkers_launched;
#if PG_VERSION_NUM >= 180000
		estate->es_parallel_workers_to_launch += pcxt->nworkers_to_launch;
		estate->es_parallel_workers_launched += pcxt->nworkers_launched;
#endif
		if (pcxt->nworkers_launched > 0)
		{
			state->readers = palloc_array(shm_mq_handle *, pcxt->nworkers_launched);
			for (int worker = 0; worker < pcxt->nworkers_launched; worker++)
			{
				shm_mq	   *queue = (shm_mq *) (state->send->queues +
												(Size) worker * GATHER_QUEUE_SIZE);

				state->readers[worker] = shm_mq_attach(queue, pcxt->seg,
													   pcxt->worker[worker].bgwhandle);
			}
			state->nreaders = pcxt->nworkers_launched;
		}
	}
	state->nextreader = 0;
	state->need_local = state->nreaders == 0 || parallel_leader_participation;
	state->local_done = !state->need_local;
	if (state->merge)
	{
		/* A source per worker, the leader's last; each loads when merged first. */
		state->nsources = state->nreaders + 1;
		state->sources = palloc0_array(MergeSource, state->nsources);
		state->sources[state->nreaders].done = !state->need_local;
		state->merge_state[0] = 0;
	}
	state->initialized = true;
}

/* A message a worker sent: its header checked against the node's. */
static void
check_message(TessGatherState *state, void *data, Size nbytes)
{
	GatherHeader *header = data;

	if (nbytes < sizeof(GatherHeader) ||
		header->ncolumns != (uint32) state->ncolumns ||
		header->key_words != (uint32) state->send->key_words ||
		header->nrows > header->stride ||
		nbytes != message_head(state->send->nlanes, header->stride) + header->values_len)
		elog(ERROR, "TessGather received a foreign message");
}

/* The next message of a worker: true with one in hand, false when every queue is empty or gone. */
static bool
gather_receive(TessGatherState *state, bool wait)
{
	for (;;)
	{
		int			visited = 0;

		while (visited < state->nreaders)
		{
			int			index = (state->nextreader + visited) % state->nreaders;
			shm_mq_handle *reader = state->readers[index];
			Size		nbytes;
			void	   *data;
			shm_mq_result result;

			visited++;
			if (reader == NULL)
				continue;
			result = shm_mq_receive(reader, &nbytes, &data, true);
			if (result == SHM_MQ_DETACHED)
			{
				shm_mq_detach(reader);
				state->readers[index] = NULL;
				continue;
			}
			if (result == SHM_MQ_WOULD_BLOCK)
				continue;
			state->nextreader = (index + 1) % state->nreaders;
			check_message(state, data, nbytes);
			state->message = data;
			state->message_rows = ((GatherHeader *) data)->nrows;
			state->message_stride = ((GatherHeader *) data)->stride;
			state->next_row = 0;
			state->messages++;
			return true;
		}
		/* Every queue gone: nothing more from the workers. */
		{
			bool		any = false;

			for (int index = 0; index < state->nreaders; index++)
				any |= state->readers[index] != NULL;
			if (!any)
				return false;
		}
		if (!wait)
			return false;
		(void) WaitLatch(MyLatch, WL_LATCH_SET | WL_EXIT_ON_PM_DEATH, 0,
						 WAIT_EVENT_EXECUTE_GATHER);
		ResetLatch(MyLatch);
		CHECK_FOR_INTERRUPTS();
	}
}

/* Whether any worker's queue is still there. */
static bool
readers_left(TessGatherState *state)
{
	for (int index = 0; index < state->nreaders; index++)
		if (state->readers[index] != NULL)
			return true;
	return false;
}

/* The next rows of the message as the batch: its columns from the lanes. */
static void
show_message_window(TessGatherState *state)
{
	uint32		n = Min(GATHER_ROWS, state->message_rows - state->next_row);
	const uint64 *lanes = (const uint64 *) (state->message + MAXALIGN(sizeof(GatherHeader)));
	const char *values = state->message + message_head(state->send->nlanes, state->message_stride);
	int			null_lanes = state->send->null_lanes;
	uint64		any[GATHER_MAX_NULL_LANES];

	for (int lane = 0; lane < null_lanes; lane++)
	{
		const uint64 *nulls = lanes + (Size) state->message_stride * lane + state->next_row;

		any[lane] = 0;
		for (uint32 row = 0; row < n; row++)
			any[lane] |= nulls[row];
	}
	for (int column = 0; column < state->ncolumns; column++)
	{
		const uint64 *lane = lanes + (Size) state->message_stride * (null_lanes + column) +
			state->next_row;
		const uint64 *nulls = lanes + (Size) state->message_stride * (column / 64) +
			state->next_row;
		bool	   *isnull = state->isnull[column];
		Datum	   *out = state->values[column];

		if ((any[column / 64] >> (column % 64)) & 1)
			for (uint32 row = 0; row < n; row++)
				isnull[row] = (nulls[row] >> (column % 64)) & 1;
		else
			memset(isnull, 0, sizeof(bool) * n);
		if (state->typbyvals[column])
			memcpy(out, lane, sizeof(Datum) * n);
		else
			for (uint32 row = 0; row < n; row++)
				out[row] = isnull[row] ? (Datum) 0 : PointerGetDatum(values + lane[row]);
	}
	state->window_start = state->next_row;
	state->next_row += n;
	state->window_bits[0] = n == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << n) - 1;
	state->batch.rows.nrows = (int) n;
	state->batch.rows.bits = state->window_bits;
	state->from_local = false;
	state->worker_rows += n;
}

/*
 * The leader runs its own part of the subtree with the query's dynamic
 * shared memory installed, as the core's Gather and Gather Merge do only
 * while they run the plan: a parallel-aware node of the core below
 * allocates there (a Parallel Bitmap Heap Scan's shared bitmap, a Parallel
 * Hash's table), and found none.
 */
static void
install_query_dsa(TessGatherState *state)
{
	state->css.ss.ps.state->es_query_dsa = state->pei != NULL ? state->pei->area : NULL;
}

static void
remove_query_dsa(TessGatherState *state)
{
	state->css.ss.ps.state->es_query_dsa = NULL;
}

/*
 * The next batch to give out: the rest of the message in hand, the next
 * message of a worker, or, while none is waiting, the leader's own; the
 * leader waits for the workers once its own part is done. False at the end.
 */
static bool
gather_next(TessGatherState *state)
{
	/* The leader's batch was consumed: the child may go on. */
	if (state->from_local && state->local_batch != NULL)
	{
		tess_input_finish(state->local);
		state->local_batch = NULL;
	}
	for (;;)
	{
		if (state->message != NULL && state->next_row < state->message_rows)
		{
			show_message_window(state);
			return true;
		}
		state->message = NULL;
		if (gather_receive(state, false))
			continue;
		if (!state->local_done)
		{
			TessBatch  *batch;

			install_query_dsa(state);
			batch = tess_input_next(state->local);
			remove_query_dsa(state);

			if (batch == NULL)
			{
				state->local_done = true;
				continue;
			}
			if (tess_row_mask_count(&batch->rows) == 0)
			{
				tess_input_finish(state->local);
				continue;
			}
			state->local_batch = batch;
			state->from_local = true;
			state->batch.rows = batch->rows;
			state->local_rows += tess_row_mask_count(&batch->rows);
			return true;
		}
		if (!readers_left(state))
			return false;
		if (gather_receive(state, true))
			continue;
		return false;
	}
}

/* ------------------------------------------------------------ TessGatherMerge */

/*
 * The next rows of source index: a worker's next message, waited for, or
 * the leader's own next rows, copied into a message as a worker's are;
 * done when it has none.
 */
static void
merge_load(TessGatherState *state, int index)
{
	MergeSource *source = &state->sources[index];
	TessSendState *send = state->send;

	source->place = 0;
	source->rows = 0;
	if (index < state->nreaders)
	{
		shm_mq_handle *reader = state->readers[index];
		Size		nbytes;
		void	   *data;

		if (shm_mq_receive(reader, &nbytes, &data, false) == SHM_MQ_DETACHED)
		{
			shm_mq_detach(reader);
			state->readers[index] = NULL;
			source->done = true;
			return;
		}
		check_message(state, data, nbytes);
		source->message = data;
		source->rows = ((GatherHeader *) data)->nrows;
		source->stride = ((GatherHeader *) data)->stride;
		source->values = source->message + message_head(send->nlanes, source->stride);
		state->messages++;
		return;
	}
	install_query_dsa(state);
	(void) fill_message(send, &state->local_builder, state->local,
						state->css.ss.ps.state->es_query_cxt);
	remove_query_dsa(state);
	if (state->local_builder.rows == 0)
	{
		source->done = true;
		return;
	}
	source->message = state->local_builder.message;
	source->values = state->local_builder.values;
	source->rows = state->local_builder.rows;
	source->stride = send->stride;
}

/* Row place of source index into the batch's arrays at out. */
static void
merge_take(TessGatherState *state, int index, uint32 place, int out)
{
	MergeSource *source = &state->sources[index];
	const uint64 *lanes = (const uint64 *) (source->message + MAXALIGN(sizeof(GatherHeader)));
	int			null_lanes = state->send->null_lanes;

	for (int column = 0; column < state->ncolumns; column++)
	{
		uint64		word = lanes[(Size) source->stride * (null_lanes + column) + place];
		bool		null = (lanes[(Size) source->stride * (column / 64) + place] >>
							(column % 64)) & 1;

		state->isnull[column][out] = null;
		if (null)
			state->values[column][out] = (Datum) 0;
		else if (state->typbyvals[column])
			state->values[column][out] = (Datum) word;
		else
			state->values[column][out] = PointerGetDatum(source->values + word);
	}
	if (index == state->nreaders)
		state->local_rows++;
	else
		state->worker_rows++;
}

/* Column `column` of source index's row place, as merge_take reads it. */
static Datum
source_value(TessGatherState *state, const MergeSource *source, int column, uint32 place,
			 bool *isnull)
{
	const uint64 *lanes = (const uint64 *) (source->message + MAXALIGN(sizeof(GatherHeader)));
	uint64		word = lanes[(Size) source->stride * (state->send->null_lanes + column) + place];

	*isnull = (lanes[(Size) source->stride * (column / 64) + place] >> (column % 64)) & 1;
	if (*isnull)
		return (Datum) 0;
	if (state->typbyvals[column])
		return (Datum) word;
	return PointerGetDatum(source->values + word);
}

/*
 * The order of two sources' next rows under a generic key: their key
 * words, then the comparisons; the binary heap keeps the greatest first,
 * so the result is reversed.
 */
static int
compare_sources(bh_node_type a, bh_node_type b, void *arg)
{
	TessGatherState *state = arg;
	TessSendState *send = state->send;
	const MergeSource *left = &state->sources[DatumGetInt32(a)];
	const MergeSource *right = &state->sources[DatumGetInt32(b)];
	const uint64 *left_lanes = (const uint64 *) (left->message + MAXALIGN(sizeof(GatherHeader)));
	const uint64 *right_lanes = (const uint64 *) (right->message + MAXALIGN(sizeof(GatherHeader)));
	int			first = send->null_lanes + state->ncolumns;

	for (int word = 0; word < send->key_words; word++)
	{
		uint64		x = left_lanes[(Size) left->stride * (first + word) + left->place];
		uint64		y = right_lanes[(Size) right->stride * (first + word) + right->place];

		if (x != y)
			return x < y ? 1 : -1;
	}
	for (int key = send->generic; key < send->nkeys; key++)
	{
		bool		left_null;
		bool		right_null;
		Datum		x = source_value(state, left, send->key_places[key], left->place, &left_null);
		Datum		y = source_value(state, right, send->key_places[key], right->place,
									 &right_null);
		int			result = ApplySortComparator(x, left_null, y, right_null, &send->ssup[key]);

		if (result != 0)
			return -result;
	}
	return 0;
}

/*
 * The merge of a generic key: rows from the source with the least next
 * row, by a binary heap made anew for every batch, until the batch is full
 * or a source's rows in hand run out with more of it to come.
 */
static int
merge_generic(TessGatherState *state)
{
	binaryheap *heap;
	int			taken = 0;

	/* A rescan may launch more workers than before. */
	if (state->merge_heap == NULL || state->merge_heap->bh_space < state->nsources)
	{
		MemoryContext old = MemoryContextSwitchTo(state->css.ss.ps.state->es_query_cxt);

		if (state->merge_heap != NULL)
			binaryheap_free(state->merge_heap);
		state->merge_heap = binaryheap_allocate(state->nsources, compare_sources, state);
		MemoryContextSwitchTo(old);
	}
	heap = state->merge_heap;
	binaryheap_reset(heap);
	for (int index = 0; index < state->nsources; index++)
		if (state->sources[index].place < state->sources[index].rows)
			binaryheap_add_unordered(heap, Int32GetDatum(index));
	binaryheap_build(heap);
	while (taken < GATHER_ROWS && !binaryheap_empty(heap))
	{
		int			index = DatumGetInt32(binaryheap_first(heap));
		MergeSource *source = &state->sources[index];

		merge_take(state, index, source->place++, taken++);
		if (source->place < source->rows)
			binaryheap_replace_first(heap, Int32GetDatum(index));
		else
		{
			(void) binaryheap_remove_first(heap);
			/* Its next rows come with the next batch, loaded then. */
			if (!source->done)
				break;
		}
	}
	return taken;
}

/*
 * The next batch in order: up to 64 rows merged from the sources by their
 * key words. A source whose rows ran out loads the next only here, once
 * the batch before, which may point into its rows, is consumed: the merge
 * stops at a source's last row in hand when more of it follows, and the
 * batch goes out shorter. False at the end.
 */
static bool
merge_next(TessGatherState *state)
{
	TessSendState *send = state->send;
	const uint64 *lanes[(TESS_SORT_MAX_MERGE_RUNS) * TESS_SORT_MAX_ITEM_WORDS];
	uint32		left[TESS_SORT_MAX_MERGE_RUNS];
	bool		more[TESS_SORT_MAX_MERGE_RUNS];
	uint32		order[GATHER_ROWS];
	int			taken = 0;

	if (state->nsources > TESS_SORT_MAX_MERGE_RUNS)
		elog(ERROR, "TessGatherMerge merges up to %d streams", TESS_SORT_MAX_MERGE_RUNS);
	/*
	 * The leader's own source first, as the core's Gather Merge reads: it
	 * sorts its share meanwhile, where waiting for the workers' first rows
	 * left it none of the scan.
	 */
	for (int visit = 0; visit < state->nsources; visit++)
	{
		int			index = (state->nreaders + visit) % state->nsources;
		MergeSource *source = &state->sources[index];

		if (source->done || source->place < source->rows)
			continue;
		merge_load(state, index);
	}
	if (send->generic >= 0)
		taken = merge_generic(state);
	while (send->generic < 0 && taken < GATHER_ROWS)
	{
		TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);
		int			count;
		int			refill;

		for (int index = 0; index < state->nsources; index++)
		{
			MergeSource *source = &state->sources[index];

			left[index] = source->rows - source->place;
			more[index] = !source->done;
			for (int word = 0; word < send->key_words; word++)
				lanes[index * send->key_words + word] = left[index] == 0 ? NULL :
					(const uint64 *) (source->message + MAXALIGN(sizeof(GatherHeader))) +
					(Size) source->stride * (send->null_lanes + state->ncolumns + word) +
					source->place;
		}
		tess_status_check(send->kernels->sort_merge(state->nsources, send->key_words, lanes, left,
													more, state->merge_state, order,
													GATHER_ROWS - taken, &count, &refill, &status),
						  &status);
		for (int row = 0; row < count; row++)
		{
			MergeSource *source = &state->sources[order[row]];

			merge_take(state, (int) order[row], source->place++, taken + row);
		}
		taken += count;
		if (count == 0 || refill >= 0)
			break;
	}
	if (taken == 0)
		return false;
	state->window_bits[0] = taken == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << taken) - 1;
	state->batch.rows.nrows = taken;
	state->batch.rows.bits = state->window_bits;
	state->from_local = false;
	return true;
}

/* Stop the workers and take their instrumentation, as ExecShutdownGatherWorkers does. */
static void
gather_shutdown_workers(TessGatherState *state)
{
	/*
	 * The queues go first, as ExecParallelFinish detaches the core's: a
	 * worker blocked on a full queue then sees the leader gone and stops,
	 * where waiting for it to finish would wait forever.
	 */
	if (state->readers != NULL)
	{
		for (int index = 0; index < state->nreaders; index++)
			if (state->readers[index] != NULL)
				shm_mq_detach(state->readers[index]);
		pfree(state->readers);
		state->readers = NULL;
	}
	if (state->pei != NULL)
		ExecParallelFinish(state->pei);
	state->nreaders = 0;
	state->message = NULL;
	if (state->sources != NULL)
		pfree(state->sources);
	state->sources = NULL;
	state->nsources = 0;
}

static TupleTableSlot *
gather_exec(CustomScanState *css)
{
	TessGatherState *state = (TessGatherState *) css;
	bool		rows = tess_output_request(state->output)->output_mode == TESS_OUTPUT_ROWS;

	CHECK_FOR_INTERRUPTS();
	if (!state->initialized)
		gather_launch(state);
	if (rows && state->published)
	{
		int			next = tess_row_mask_next(&state->batch.rows, state->served);

		if (next >= 0)
		{
			state->served = next;
			return tess_output_select(state->output, next);
		}
		tess_output_finish(state->output);
	}
	tess_output_release(state->output);
	state->published = false;
	if (!(state->merge ? merge_next(state) : gather_next(state)))
		return NULL;
	state->published = true;
	state->served = tess_row_mask_next(&state->batch.rows, -1);
	return tess_output_publish(state->output, &state->batch);
}

/* As ExecShutdownGather: the workers stop, and their instrumentation comes into the plan. */
static void
gather_shutdown(CustomScanState *css)
{
	TessGatherState *state = (TessGatherState *) css;

	gather_shutdown_workers(state);
	if (state->pei != NULL)
	{
		ExecParallelCleanup(state->pei);
		state->pei = NULL;
	}
}

static void
gather_end(CustomScanState *css)
{
	TessGatherState *state = (TessGatherState *) css;

	tess_output_end(state->output);
	ExecEndNode(&state->send->css.ss.ps);
	gather_shutdown_workers(state);
	if (state->pei != NULL)
	{
		ExecParallelCleanup(state->pei);
		state->pei = NULL;
	}
}

/* As ExecReScanGather: the workers stop, the subtree rescans, and the next execution launches anew. */
static void
gather_rescan(CustomScanState *css)
{
	TessGatherState *state = (TessGatherState *) css;

	tess_output_clear(state->output);
	state->published = false;
	state->local_batch = NULL;
	if (state->local_builder.batch != NULL)
		state->local_builder.batch = NULL;
	state->local_builder.exhausted = false;
	gather_shutdown_workers(state);
	state->initialized = false;
	state->from_local = false;
	ExecReScan(state->send->child);
	tess_input_rescan(state->local);
}

static void
gather_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessGatherState *state = (TessGatherState *) css;

	ExplainPropertyInteger("Workers Planned", NULL, state->num_workers, es);
	if (!es->analyze)
		return;
	ExplainPropertyInteger("Workers Launched", NULL, state->nworkers_launched, es);
	/* How the participants shared the rows varies from run to run: VERBOSE only. */
	if (!es->verbose)
		return;
	ExplainPropertyInteger("Messages", NULL, state->messages, es);
	ExplainPropertyInteger("Rows from Workers", NULL, state->worker_rows, es);
	ExplainPropertyInteger("Rows of the Leader", NULL, state->local_rows, es);
}

/* As ExecSetTupleBound for a Gather: the leader's part and every worker's need no more rows. */
static void
gather_set_tuple_bound(CustomScanState *css, int64 tuples_needed)
{
	TessGatherState *state = (TessGatherState *) css;

	state->bound = tuples_needed < 0 ? -1 : tuples_needed;
	tess_set_child_bound(state->send->child, state->bound);
}

static const CustomExecMethods gather_exec_methods = {
	.CustomName = "TessGather",
	.BeginCustomScan = gather_begin,
	.ExecCustomScan = gather_exec,
	.EndCustomScan = gather_end,
	.ReScanCustomScan = gather_rescan,
	.ExplainCustomScan = gather_explain,
	.ShutdownCustomScan = gather_shutdown,
};

const CustomScanMethods tess_gather_scan_methods = {
	.CustomName = "TessGather",
	.CreateCustomScanState = gather_create_state,
};

static const CustomExecMethods gather_merge_exec_methods = {
	.CustomName = "TessGatherMerge",
	.BeginCustomScan = gather_begin,
	.ExecCustomScan = gather_exec,
	.EndCustomScan = gather_end,
	.ReScanCustomScan = gather_rescan,
	.ExplainCustomScan = gather_explain,
	.ShutdownCustomScan = gather_shutdown,
};

static Node *
gather_merge_create_state(CustomScan *cscan)
{
	TessGatherState *state = (TessGatherState *) newNode(sizeof(TessGatherState), T_CustomScanState);

	state->css.methods = &gather_merge_exec_methods;
	return (Node *) state;
}

const CustomScanMethods tess_gather_merge_scan_methods = {
	.CustomName = "TessGatherMerge",
	.CreateCustomScanState = gather_merge_create_state,
};

const CustomScanMethods tess_send_scan_methods = {
	.CustomName = "TessSend",
	.CreateCustomScanState = send_create_state,
};

const TessNode tess_gather_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_GATHER_NODE_NAME,
	.set_tuple_bound = gather_set_tuple_bound,
};

const TessNode tess_gather_merge_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_GATHER_MERGE_NODE_NAME,
	.set_tuple_bound = gather_set_tuple_bound,
};

const TessNode tess_send_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_SEND_NODE_NAME,
};

void
tess_gather_planner_init(void)
{
	previous_create_upper_paths_hook = create_upper_paths_hook;
	create_upper_paths_hook = create_upper_paths;
	previous_planner_hook = planner_hook;
	planner_hook = gather_planner;
}

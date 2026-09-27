#include "postgres.h"

#include "access/parallel.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/execParallel.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/planner.h"
#include "pgstat.h"
#include "storage/dsm.h"
#include "storage/latch.h"
#include "storage/proc.h"
#include "storage/shm_mq.h"
#include "utils/datum.h"
#include "utils/memutils.h"
#include "utils/wait_event.h"

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
 * GATHER_MESSAGE_ROWS rows, a lane of their NULL bits, a lane of words per
 * column and the bytes of the by-reference values, each column's word its
 * value or the value's byte in the message, and sends each whole through
 * a queue of its own in the node's chunk of the query's shared memory. The
 * leader's TessGather launches the workers as the core's Gather does, and
 * gives its parent the messages as batches of up to 64 rows whose columns
 * point into the message; while every queue is empty and the leader takes
 * part, it runs the subtree itself, reading TessSend's child directly.
 */

/* Rows of a batch given out, and the most rows of a message. */
#define GATHER_ROWS 64
#define GATHER_MESSAGE_ROWS 1024
/* A queue per worker, as large as four of the core's tuple queues. */
#define GATHER_QUEUE_SIZE (256 * 1024)

/* What a message starts with; its lanes and values follow, aligned to 8. */
typedef struct GatherHeader
{
	uint32		nrows;
	uint32		ncolumns;
	/* The rows each lane has room for: the lanes' stride. */
	uint32		stride;
	uint32		pad;
	uint64		values_len;
} GatherHeader;

/* The part of TessSend's chunk before the queues. */
typedef struct SendShared
{
	dsm_handle	segment;
	int			nqueues;
} SendShared;

#define SEND_QUEUES_OFFSET MAXALIGN(sizeof(SendShared))

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
	/* The rows of a message: a quarter of the queue for its lanes, 64 to GATHER_MESSAGE_ROWS. */
	uint32		stride;
	/* The leader's view of the queues, once laid out. */
	SendShared *shared;
	char	   *queues;
	/* A worker's queue and the message it fills. */
	shm_mq_handle *queue;
	char	   *message;
	Size		message_len;
	uint32		rows;
	char	   *values;
	Size		values_len;
	Size		values_used;
	uint64		sent_messages;
	uint64		sent_rows;
	bool		done;
} TessSendState;

typedef struct TessGatherState
{
	CustomScanState css;
	TessSendState  *send;
	int			num_workers;
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
} TessGatherState;

static const CustomExecMethods send_exec_methods;
static const CustomExecMethods gather_exec_methods;
static create_upper_paths_hook_type previous_create_upper_paths_hook = NULL;

static Plan *gather_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
						 List *tlist, List *clauses, List *custom_plans);
static Plan *send_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
					   List *tlist, List *clauses, List *custom_plans);

static const CustomPathMethods gather_path_methods = {
	.CustomName = "TessGather",
	.PlanCustomPath = gather_plan,
};

static const CustomPathMethods send_path_methods = {
	.CustomName = "TessSend",
	.PlanCustomPath = send_plan,
};

/* ---------------------------------------------------------------- planning */

/*
 * The node's path in place of a Gather over a batch path: TessGather over
 * TessSend over the Gather's subpath, with the Gather's rows and costs.
 * NULL where the core's Gather stays: a single copy, no worker, a subpath
 * that is no batch path, or a Gather that projects.
 */
static Path *
make_gather_path(PlannerInfo *root, GatherPath *gather)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *subpath = gather->subpath;
	CustomPath *send;
	CustomPath *path;

	if (gather->single_copy || gather->num_workers <= 0 ||
		tess_path_node(subpath) == NULL || !subpath->parallel_safe ||
		subpath->param_info != NULL ||
		!equal(gather->path.pathtarget->exprs, subpath->pathtarget->exprs) ||
		list_length(subpath->pathtarget->exprs) == 0 ||
		list_length(subpath->pathtarget->exprs) > 64)
		return NULL;
	config.template_path = subpath;
	config.methods = &send_path_methods;
	config.node = &tess_send_node;
	config.children = list_make1(subpath);
	send = tess_path_create(&config);
	/* Parallel-aware for the chunk of shared memory its queues take. */
	send->path.parallel_aware = true;
	config = (TessPathConfig) TESS_STRUCT_INITIALIZER(TessPathConfig);
	config.template_path = &gather->path;
	config.methods = &gather_path_methods;
	config.node = &tess_gather_node;
	config.children = list_make1(&send->path);
	config.node_data = (Node *) list_make1(makeInteger(gather->num_workers));
	path = tess_path_create(&config);
	return &path->path;
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
					tess_path_node(linitial(custom->custom_paths)) == &tess_gather_node &&
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

	config.methods = &tess_send_scan_methods;
	config.layout_policy = TESS_LAYOUT_PRESERVE_CHILD;
	config.layout_child = 0;
	config.scanrelid = 0;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

/* TessGather: a column per target, the targets TessSend sends. */
static Plan *
gather_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path, List *tlist,
			List *clauses, List *custom_plans)
{
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanWriter *writer;

	tess_path_get_info(best_path, &info);
	/* The plan launches workers, as the core's Gather's does. */
	root->glob->parallelModeNeeded = true;
	writer = tess_plan_writer_create(TESS_GATHER_DATA, TESS_GATHER_DATA_VERSION);
	tess_plan_write_int(writer, "workers", intVal(linitial((List *) info.node_data)));
	config.methods = &tess_gather_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

/* ---------------------------------------------------------------- TessSend */

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

	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_send_node || info.nchildren != 1 ||
		info.child_names[0] == NULL)
		elog(ERROR, "TessSend received a foreign plan");
	state->child = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->child_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	tess_plan_get_layout(state->child->plan, &state->child_layout);
	state->ncolumns = desc->natts;
	if (state->ncolumns != state->child_layout.ntargets || state->ncolumns > 64)
		elog(ERROR, "TessSend received a foreign plan");
	state->typlens = palloc_array(int16, Max(state->ncolumns, 1));
	state->typbyvals = palloc_array(bool, Max(state->ncolumns, 1));
	for (int column = 0; column < state->ncolumns; column++)
	{
		state->typlens[column] = TupleDescAttr(desc, column)->attlen;
		state->typbyvals[column] = TupleDescAttr(desc, column)->attbyval;
	}
	state->stride = (uint32) Min((Size) GATHER_MESSAGE_ROWS,
								 Max((Size) GATHER_ROWS,
									 GATHER_QUEUE_SIZE / 4 /
									 (sizeof(uint64) * (1 + state->ncolumns))));
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

/* The bytes of a message of rows rows before its values. */
static Size
message_head(int ncolumns, uint32 rows)
{
	return MAXALIGN(sizeof(GatherHeader)) + sizeof(uint64) * (Size) rows * (1 + ncolumns);
}

/* Send the message the worker filled, whole; false once the leader left. */
static bool
send_message(TessSendState *state)
{
	GatherHeader *header = (GatherHeader *) state->message;
	Size		head = message_head(state->ncolumns, state->stride);
	shm_mq_iovec parts[2];
	shm_mq_result result;

	if (state->rows == 0)
		return true;
	header->nrows = state->rows;
	header->ncolumns = state->ncolumns;
	header->stride = state->stride;
	header->values_len = state->values_used;
	/* The lanes as filled, for stride rows each, then the values. */
	parts[0].data = state->message;
	parts[0].len = head;
	parts[1].data = state->values;
	parts[1].len = state->values_used;
	result = shm_mq_sendv(state->queue, parts, state->values_used > 0 ? 2 : 1, false, true);
	state->sent_messages++;
	state->sent_rows += state->rows;
	state->rows = 0;
	state->values_used = 0;
	return result == SHM_MQ_SUCCESS;
}

/* A worker: every batch of the child into messages, sent as they fill. */
static TupleTableSlot *
send_exec(CustomScanState *css)
{
	TessSendState  *state = (TessSendState *) css;
	TessDatumColumn columns[64];

	if (state->input == NULL)
		elog(ERROR, "TessSend runs only in a parallel worker");
	if (state->done)
		return NULL;
	if (state->queue == NULL)
		elog(ERROR, "TessSend has no queue");
	if (state->message == NULL)
	{
		MemoryContext context = css->ss.ps.state->es_query_cxt;

		state->message_len = message_head(state->ncolumns, state->stride);
		state->message = MemoryContextAllocZero(context, state->message_len);
		state->values_len = 64 * 1024;
		state->values = MemoryContextAlloc(context, state->values_len);
	}
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->input);
		int			row = -1;

		if (batch == NULL)
			break;
		for (int column = 0; column < state->ncolumns; column++)
		{
			columns[column] = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
			batch->ops->get_datum_column(batch,
										 tess_layout_column(&state->child_layout, column),
										 &batch->rows, TESS_COLUMN_FOR_PROJECTION,
										 &columns[column]);
			if (columns[column].values == NULL || columns[column].isnull == NULL ||
				columns[column].nrows != batch->rows.nrows)
				elog(ERROR, "Tessera batch returned an invalid column");
		}
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		{
			uint64	   *lanes = (uint64 *) (state->message + MAXALIGN(sizeof(GatherHeader)));
			uint64		nulls = 0;
			Size		need = 0;

			for (int column = 0; column < state->ncolumns; column++)
				if (!columns[column].isnull[row] && !state->typbyvals[column])
					need += MAXALIGN(datumGetSize(columns[column].values[row], false,
												  state->typlens[column]));
			if (state->rows == state->stride ||
				(state->rows > 0 && state->values_used + need > GATHER_QUEUE_SIZE / 2))
			{
				if (!send_message(state))
				{
					state->done = true;
					tess_input_finish(state->input);
					return NULL;
				}
			}
			if (state->values_used + need > state->values_len)
			{
				state->values_len = Max(state->values_len * 2, state->values_used + need);
				state->values = repalloc_huge(state->values, state->values_len);
			}
			for (int column = 0; column < state->ncolumns; column++)
			{
				uint64	   *lane = lanes + (Size) state->stride * (1 + column);

				if (columns[column].isnull[row])
				{
					nulls |= UINT64CONST(1) << column;
					lane[state->rows] = 0;
				}
				else if (state->typbyvals[column])
					lane[state->rows] = (uint64) columns[column].values[row];
				else
				{
					Size		size = datumGetSize(columns[column].values[row], false,
													state->typlens[column]);

					memcpy(state->values + state->values_used,
						   DatumGetPointer(columns[column].values[row]), size);
					lane[state->rows] = state->values_used;
					state->values_used += MAXALIGN(size);
				}
			}
			lanes[state->rows] = nulls;
			state->rows++;
		}
		tess_input_finish(state->input);
		CHECK_FOR_INTERRUPTS();
	}
	(void) send_message(state);
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
	if (info.node != &tess_gather_node || info.nchildren != 1 ||
		info.child_names[0] == NULL)
		elog(ERROR, "TessGather received a foreign plan");
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
	if (state->ncolumns != state->send->ncolumns || state->ncolumns > 64)
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
		if (state->pei == NULL)
			state->pei = ExecInitParallelPlan(send, estate, params, state->num_workers, -1);
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
	state->initialized = true;
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
			state->message = data;
			state->message_rows = ((GatherHeader *) data)->nrows;
			state->message_stride = ((GatherHeader *) data)->stride;
			if (nbytes < sizeof(GatherHeader) ||
				((GatherHeader *) data)->ncolumns != (uint32) state->ncolumns ||
				state->message_rows > state->message_stride ||
				nbytes != message_head(state->ncolumns, state->message_stride) +
				((GatherHeader *) data)->values_len)
				elog(ERROR, "TessGather received a foreign message");
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
	const char *values = state->message + message_head(state->ncolumns, state->message_stride);
	const uint64 *nulls = lanes + state->next_row;
	uint64		any = 0;

	for (uint32 row = 0; row < n; row++)
		any |= nulls[row];
	for (int column = 0; column < state->ncolumns; column++)
	{
		const uint64 *lane = lanes + (Size) state->message_stride * (1 + column) + state->next_row;
		bool	   *isnull = state->isnull[column];
		Datum	   *out = state->values[column];

		if ((any >> column) & 1)
			for (uint32 row = 0; row < n; row++)
				isnull[row] = (nulls[row] >> column) & 1;
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
			TessBatch  *batch = tess_input_next(state->local);

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

/* Stop the workers and take their instrumentation, as ExecShutdownGatherWorkers does. */
static void
gather_shutdown_workers(TessGatherState *state)
{
	if (state->pei != NULL)
		ExecParallelFinish(state->pei);
	if (state->readers != NULL)
	{
		for (int index = 0; index < state->nreaders; index++)
			if (state->readers[index] != NULL)
				shm_mq_detach(state->readers[index]);
		pfree(state->readers);
		state->readers = NULL;
	}
	state->nreaders = 0;
	state->message = NULL;
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
	if (!gather_next(state))
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
	if (state->local_batch != NULL)
		state->local_batch = NULL;
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

const CustomScanMethods tess_send_scan_methods = {
	.CustomName = "TessSend",
	.CreateCustomScanState = send_create_state,
};

const TessNode tess_gather_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_GATHER_NODE_NAME,
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
}

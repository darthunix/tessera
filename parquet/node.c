/*
 * The module's batch source and its node, TessParquetScan. The source
 * reads every table of the wrapper the scan planner offers it; the node
 * publishes batches of up to 64 rows of one window of a group, and makes
 * the PostgreSQL values of a column only when a consumer asks for it, for
 * the rows it asks for, keeping them until the next batch. Each column is
 * read by one reader a group, forward window by window, or, for a window
 * after a gap, for the rows asked alone.
 */
#include "postgres.h"

#include "access/sysattr.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "commands/explain_state.h"
#include "executor/executor.h"
#include "fmgr.h"
#include "foreign/fdwapi.h"
#include "miscadmin.h"
#include "nodes/extensible.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "port/pg_bitutils.h"
#include "utils/injection_point.h"
#include "utils/memutils.h"
#include "utils/rel.h"

#include "tessera/bridge.h"
#include "tessera/plan.h"
#include "tessera/planner.h"
#include "tessera/runtime.h"
#include "tessera/source.h"

#include "tessera_parquet.h"

PGDLLEXPORT void _PG_init(void);

#define NODE_NAME "tessera_parquet.scan"
#define DATA_KIND "tessera_parquet.scan"
#define DATA_VERSION 1
#define BATCH_ROWS 64
#define WINDOW_WORDS (PARQUET_WINDOW_ROWS / 64)
#define NO_WINDOW PG_UINT64_MAX

typedef struct ParquetNode ParquetNode;

/* The batch the node publishes: rows of one window of a group. */
typedef struct ParquetBatch
{
	TessBatch	batch;
	ParquetNode *node;
	uint64		start;			/* its first row in the group */
	int			nrows;
	uint64		mask;			/* its rows selected, which a consumer narrows */
	uint64		all;			/* every row of it, BATCH_ROWS at most */
	Datum	  **values;			/* of each column of the relation asked */
	bool	  **isnull;
	uint64	   *made;			/* of each column, the rows made */
} ParquetBatch;

/* What the node keeps of a column it reads: the window its reader holds. */
typedef struct NodeColumn
{
	uint64		window;			/* the window's first row, or NO_WINDOW */
	uint64		forward_end;	/* where the forward reads ended, or NO_WINDOW */
	uint64		held[WINDOW_WORDS]; /* the window's rows the read holds */
	uint64		values[PARQUET_WINDOW_ROWS];	/* of the rows held, in order */
	uint64		nulls[WINDOW_WORDS];
} NodeColumn;

struct ParquetNode
{
	CustomScanState css;
	List	   *planned;		/* parquet_planned_list's */
	ParquetScan *scan;			/* NULL under EXPLAIN alone */
	int		   *column_of;		/* of each column of the relation, the scan's */
	NodeColumn *columns;
	TessLayout	layout;
	TessLayout	relation;
	TessProjection *projection;
	TessOutput *output;
	const TessRequest *request;
	int			capacity;
	bool		exhausted;
	uint64		next_row;		/* of the group read, the next to publish */
	MemoryContext batch_cxt;	/* the values of the batch, reset at the next */
	ParquetBatch batch;
	uint64		values_decoded; /* over every rescan */
	/* A row-wise parent: the batch served, its next row, its targets. */
	bool		rows;
	TessBatch  *active;
	int			next_active_row;
	TessDatumColumn *targets;
};

static Plan *plan_path(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
					   List *tlist, List *clauses, List *custom_plans);
static Node *create_state(CustomScan *cscan);
static void node_begin(CustomScanState *css, EState *estate, int eflags);
static TupleTableSlot *node_exec(CustomScanState *css);
static void node_end(CustomScanState *css);
static void node_rescan(CustomScanState *css);
static void node_explain(CustomScanState *css, List *ancestors, ExplainState *es);
static void batch_column(TessBatch *batch, int column, const TessRowMask *rows,
						 TessColumnPurpose purpose, TessDatumColumn *result);
static CustomPath *scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses);

static const TessNode parquet_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = NODE_NAME,
};

static const TessSource parquet_source = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_parquet",
	.scan_rows = scan_rows,
};

static const CustomPathMethods path_methods = {
	.CustomName = "TessParquetScan",
	.PlanCustomPath = plan_path,
};

static const CustomScanMethods scan_methods = {
	.CustomName = "TessParquetScan",
	.CreateCustomScanState = create_state,
};

static const CustomExecMethods exec_methods = {
	.CustomName = "TessParquetScan",
	.BeginCustomScan = node_begin,
	.ExecCustomScan = node_exec,
	.EndCustomScan = node_end,
	.ReScanCustomScan = node_rescan,
	.ExplainCustomScan = node_explain,
};

static const TessBatchOps batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = batch_column,
};

/* ---- the source ---- */

/*
 * The node's path for the wrapper's serial scan: the plan carries what
 * planning found. It checks no clause yet, and offers no partial path.
 */
static CustomPath *
scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	TessPlanWriter *writer;
	CustomPath *scan;
	Bitmapset  *attrs = NULL;
	int			first;

	if (!IsA(path, ForeignPath) || path->parent->fdwroutine == NULL ||
		path->parent->fdwroutine->GetForeignPaths != parquet_get_foreign_paths ||
		path->parallel_workers > 0)
		return NULL;
	/* The projection reads columns alone: a whole row or a system column is
	 * the wrapper's to read. */
	pull_varattnos((Node *) path->pathtarget->exprs, path->parent->relid, &attrs);
	first = bms_next_member(attrs, -1);
	if (first >= 0 && first + FirstLowInvalidHeapAttributeNumber <= 0)
		return NULL;
	writer = tess_plan_writer_create(DATA_KIND, DATA_VERSION);
	tess_plan_write_list(writer, "planned", parquet_planned_list(path->parent->fdw_private));
	config.template_path = path;
	config.methods = &path_methods;
	config.node = &parquet_node;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	scan = tess_path_create(&config);
	/*
	 * shortcut: the wrapper's costs less a tenth, so that the node is
	 * chosen; the costs of both paths from the footers come with their
	 * estimates.
	 */
	scan->path.startup_cost = path->startup_cost * 0.9;
	scan->path.total_cost = path->total_cost * 0.9;
	return scan;
}

static Plan *
plan_path(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		  List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);

	tess_path_get_info(best_path, &info);
	layout.ncolumns = rel->max_attr;
	layout.ntargets = rel->max_attr;
	config.methods = &scan_methods;
	config.layout_policy = TESS_LAYOUT_PROJECTED;
	config.explicit_layout = &layout;
	config.expressions = info.expressions;
	config.node_data = info.node_data;
	config.scanrelid = rel->relid;
	config.scan_tuple_is_relation = true;
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

/* ---- the node ---- */

static Node *
create_state(CustomScan *cscan)
{
	ParquetNode *node = (ParquetNode *) newNode(sizeof(ParquetNode), T_CustomScanState);

	node->css.methods = &exec_methods;
	return (Node *) node;
}

static void
node_begin(CustomScanState *css, EState *estate, int eflags)
{
	ParquetNode *node = (ParquetNode *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	Relation	rel = css->ss.ss_currentRelation;
	TessPlanReader *reader;
	List	   *attnums;
	int			natts;
	int			c = 0;

	tess_node_require_forward(eflags, "TessParquetScan");
	parquet_register();
	tess_plan_get_info(cscan, &info);
	if (info.node == NULL || strcmp(info.node->name, NODE_NAME) != 0 ||
		info.nchildren != 0 || rel == NULL || cscan->custom_scan_tlist != NIL)
		elog(ERROR, "TessParquetScan received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, DATA_KIND, DATA_VERSION);
	node->planned = tess_plan_read_list(reader, "planned");
	tess_plan_reader_finish(reader);
	natts = RelationGetDescr(rel)->natts;
	node->layout = info.layout;
	node->relation = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	node->relation.ncolumns = natts;
	node->relation.ntargets = natts;
	if (info.computed != NIL)
		node->projection = tess_node_projection(css, css->ss.ss_ScanTupleSlot, &node->relation,
												natts, info.computed);
	node->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									  css->ss.ps.ps_ResultTupleSlot, &info.layout);
	node->batch_cxt = AllocSetContextCreate(estate->es_query_cxt, "TessParquetScan batch",
											ALLOCSET_DEFAULT_SIZES);
	node->batch.node = node;
	node->batch.batch.abi_version = TESS_BATCH_ABI_VERSION;
	node->batch.batch.struct_size = sizeof(TessBatch);
	node->batch.batch.table_oid = RelationGetRelid(rel);
	node->batch.batch.ops = &batch_ops;
	node->batch.batch.private_data = &node->batch;
	node->batch.batch.rows.bits = &node->batch.mask;
	/* A read of a column can fail on a value, text not UTF-8 say. */
	node->batch.batch.values_stored = false;
	if (eflags & EXEC_FLAG_EXPLAIN_ONLY)
		return;
	attnums = lsecond(node->planned);
	node->scan = parquet_scan_begin(RelationGetRelid(rel), parquet_strings(linitial(node->planned)),
									attnums);
	node->column_of = palloc_array(int, Max(natts, 1));
	for (int a = 0; a < natts; a++)
		node->column_of[a] = -1;
	foreach_int(attnum, attnums)
		node->column_of[attnum - 1] = c++;
	node->columns = palloc0_array(NodeColumn, Max(node->scan->ncolumns, 1));
}

/* The rank of the window's row r among the rows held: its place in values. */
static int
rank_of(const uint64 *held, int r)
{
	int			rank = 0;

	for (int w = 0; w < r / 64; w++)
		rank += pg_popcount64(held[w]);
	if (r % 64 != 0)
		rank += pg_popcount64(held[r / 64] & ((UINT64CONST(1) << (r % 64)) - 1));
	return rank;
}

/* Whether every row of rows is held. */
static bool
all_held(const uint64 *rows, const uint64 *held)
{
	for (int w = 0; w < WINDOW_WORDS; w++)
	{
		if ((rows[w] & ~held[w]) != 0)
			return false;
	}
	return true;
}

/*
 * Makes the values of the rows `need` of the batch's column `column`, the
 * scan's column `c`: the batch's window read where the column's reader
 * does not hold those rows, forward where the window follows the last
 * forward one or is the first, and for those rows alone otherwise.
 */
static void
make_values(ParquetNode *node, ParquetBatch *b, int column, int c, uint64 need)
{
	ParquetScan *scan = node->scan;
	NodeColumn *col = &node->columns[c];
	uint64		window = b->start / PARQUET_WINDOW_ROWS * PARQUET_WINDOW_ROWS;
	size_t		len = (size_t) Min((uint64) PARQUET_WINDOW_ROWS,
								   parquet_scan_group_rows(scan) - window);
	int			offset = (int) (b->start - window);
	uint64		rows[WINDOW_WORDS] = {0};
	uint64		asked[WINDOW_WORDS] = {0};
	TessParquetReader *reader = parquet_scan_reader(scan, c);
	TessParquetStatus status;
	size_t		count;
	int			held_words;

	rows[offset / 64] |= need << (offset % 64);
	if (offset % 64 != 0 && offset / 64 + 1 < WINDOW_WORDS)
		rows[offset / 64 + 1] |= need >> (64 - offset % 64);
	if (col->window != window || !all_held(rows, col->held))
	{
		bool		forward = col->forward_end == NO_WINDOW || col->forward_end == window;

		memset(col->held, 0, sizeof(col->held));
		if (!tess_parquet_read(reader, window, len, rows, (len + 63) / 64, col->held,
							   (len + 63) / 64, &count, &status))
			parquet_raise(&status);
		col->window = window;
		if (forward)
		{
			col->forward_end = window + len;
			if (count != len)
				ereport(ERROR,
						(errcode(ERRCODE_DATA_CORRUPTED),
						 errmsg("Parquet column holds fewer rows than its group")));
		}
		INJECTION_POINT("tessera-parquet-window", NULL);
	}
	count = 0;
	for (int w = 0; w < WINDOW_WORDS; w++)
		count += pg_popcount64(col->held[w]);
	held_words = (int) Max((count + 63) / 64, 1);
	for (uint64 bits = need; bits != 0; bits &= bits - 1)
	{
		int			p = rank_of(col->held, offset + pg_rightmost_one_pos64(bits));

		asked[p / 64] |= UINT64CONST(1) << (p % 64);
	}
	if (scan->info[c].gives_datums)
	{
		if (!tess_parquet_values(reader, asked, held_words, col->values, Max(count, 1),
								 col->nulls, held_words, &status))
			parquet_raise(&status);
	}
	else
	{
		size_t		bytes = 0;
		uint8	   *arena;

		if (!tess_parquet_text_size(reader, asked, held_words, col->nulls, held_words, &bytes,
									&status))
			parquet_raise(&status);
		arena = MemoryContextAlloc(node->batch_cxt, Max(bytes, 1));
		if (!tess_parquet_text(reader, arena, bytes, col->values, Max(count, 1), &status))
			parquet_raise(&status);
	}
	for (uint64 bits = need; bits != 0; bits &= bits - 1)
	{
		int			i = pg_rightmost_one_pos64(bits);
		int			p = rank_of(col->held, offset + i);
		bool		null = (col->nulls[p / 64] >> (p % 64)) & 1;

		b->isnull[column][i] = null;
		b->values[column][i] = null ? (Datum) 0 : (Datum) col->values[p];
	}
	b->made[column] |= need;
	node->values_decoded += pg_popcount64(need);
}

/* A column of the batch, its values made for the rows asked not made yet. */
static void
batch_column(TessBatch *batch, int column, const TessRowMask *rows,
			 TessColumnPurpose purpose, TessDatumColumn *result)
{
	ParquetBatch *b = batch->private_data;
	ParquetNode *node = b->node;
	uint64		need;

	if (column < 0 || column >= node->relation.ncolumns || node->column_of[column] < 0)
		elog(ERROR, "TessParquetScan was asked for a column its plan does not read");
	if (b->values[column] == NULL)
	{
		b->values[column] = MemoryContextAllocZero(node->batch_cxt,
												   sizeof(Datum) * Max(b->nrows, 1));
		b->isnull[column] = MemoryContextAllocZero(node->batch_cxt,
												   sizeof(bool) * Max(b->nrows, 1));
	}
	/* A row asked is made even when a consumer took it out of the mask. */
	need = rows->bits[0] & b->all & ~b->made[column];
	if (need != 0)
		make_values(node, b, column, node->column_of[column], need);
	result->values = b->values[column];
	result->isnull = b->isnull[column];
	result->nrows = b->nrows;
	if (result->struct_size >= TESS_DATUM_COLUMN_DECIMALS_SIZE)
	{
		result->decimal_rows = NULL;
		result->decimal_scale = 0;
	}
}

/*
 * The next batch: up to the capacity's rows of the group read, within one
 * window, or NULL past the last group. A query that reads no column opens
 * no file and reads no page.
 */
static TessBatch *
next_batch(ParquetNode *node)
{
	ParquetScan *scan = node->scan;
	ParquetBatch *b = &node->batch;
	uint64		rows;
	uint64		end;
	int			natts = node->relation.ncolumns;

	if (node->exhausted)
		return NULL;
	while (!scan->group_started || node->next_row >= parquet_scan_group_rows(scan))
	{
		CHECK_FOR_INTERRUPTS();
		if (!parquet_scan_next_group(scan))
		{
			node->exhausted = true;
			return NULL;
		}
		node->next_row = 0;
		for (int c = 0; c < scan->ncolumns; c++)
		{
			node->columns[c].window = NO_WINDOW;
			node->columns[c].forward_end = NO_WINDOW;
		}
	}
	rows = parquet_scan_group_rows(scan);
	end = Min(rows, (node->next_row / PARQUET_WINDOW_ROWS + 1) * PARQUET_WINDOW_ROWS);
	MemoryContextReset(node->batch_cxt);
	b->start = node->next_row;
	b->nrows = (int) Min((uint64) node->capacity, end - node->next_row);
	b->all = b->nrows == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << b->nrows) - 1;
	b->mask = b->all;
	b->values = MemoryContextAllocZero(node->batch_cxt, sizeof(Datum *) * Max(natts, 1));
	b->isnull = MemoryContextAllocZero(node->batch_cxt, sizeof(bool *) * Max(natts, 1));
	b->made = MemoryContextAllocZero(node->batch_cxt, sizeof(uint64) * Max(natts, 1));
	b->batch.rows.nrows = b->nrows;
	node->next_row += b->nrows;
	if (node->projection != NULL)
		return tess_projection_wrap(node->projection, &b->batch);
	return &b->batch;
}

static void
drop_active(ParquetNode *node)
{
	if (node->active != NULL && node->projection != NULL)
		node->active->ops->release(node->active);
	node->active = NULL;
}

/* A row-wise parent: the next row of the batch served. */
static TupleTableSlot *
exec_rows(ParquetNode *node)
{
	TupleTableSlot *slot = node->css.ss.ps.ps_ResultTupleSlot;

	for (;;)
	{
		int			row;

		if (node->active == NULL)
		{
			TessBatch  *batch = next_batch(node);

			if (batch == NULL)
				return NULL;
			tess_batch_target_columns(batch, &node->layout, node->layout.ntargets,
									  node->targets);
			node->active = batch;
			node->next_active_row = tess_row_mask_next(&batch->rows, -1);
		}
		if (node->next_active_row < 0)
		{
			drop_active(node);
			continue;
		}
		row = node->next_active_row;
		node->next_active_row = tess_row_mask_next(&node->active->rows, row);
		ExecClearTuple(slot);
		for (int target = 0; target < node->layout.ntargets; target++)
		{
			slot->tts_values[target] = node->targets[target].values[row];
			slot->tts_isnull[target] = node->targets[target].isnull[row];
		}
		slot->tts_tableOid = node->active->table_oid;
		return ExecStoreVirtualTuple(slot);
	}
}

static TupleTableSlot *
node_exec(CustomScanState *css)
{
	ParquetNode *node = (ParquetNode *) css;
	TessBatch  *batch;

	if (node->request == NULL)
	{
		node->request = tess_output_request(node->output);
		node->capacity = node->request->max_batch_rows > 0 ?
			Min(node->request->max_batch_rows, BATCH_ROWS) : BATCH_ROWS;
		node->rows = node->request->output_mode == TESS_OUTPUT_ROWS;
		if (node->rows)
			node->targets = palloc0_array(TessDatumColumn, Max(node->layout.ntargets, 1));
	}
	if (node->rows)
		return exec_rows(node);
	tess_output_release(node->output);
	batch = next_batch(node);
	if (batch == NULL)
		return NULL;
	return tess_output_publish(node->output, batch);
}

static void
node_end(CustomScanState *css)
{
	ParquetNode *node = (ParquetNode *) css;

	drop_active(node);
	tess_output_end(node->output);
	if (node->scan != NULL)
		parquet_scan_end(node->scan);
}

/* The next scan reads every file again, as it was when the scan began. */
static void
node_rescan(CustomScanState *css)
{
	ParquetNode *node = (ParquetNode *) css;

	tess_output_clear(node->output);
	drop_active(node);
	if (node->projection != NULL)
		tess_projection_reset(node->projection);
	MemoryContextReset(node->batch_cxt);
	if (node->scan != NULL)
		parquet_scan_rescan(node->scan);
	node->exhausted = false;
	node->next_row = 0;
}

static void
node_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	ParquetNode *node = (ParquetNode *) css;

	parquet_explain_files(node->planned, node->scan, es);
	if (es->analyze && node->scan != NULL)
	{
		ExplainPropertyUInteger("Row Groups Skipped", NULL, 0, es);
		ExplainPropertyUInteger("Values Decoded", NULL, node->values_decoded, es);
	}
}

/*
 * Registers the node and the source once the bridge is loaded: when the
 * module loads, and again when a table is planned and when a scan begins,
 * so that a bridge loaded after the module, in a backend or a worker,
 * still gets them. Without the bridge the wrapper reads its rows alone.
 */
void
parquet_register(void)
{
	static bool registered = false;
	const TessApi *api;

	if (registered || *find_rendezvous_variable(TESS_API_RENDEZVOUS) == NULL)
		return;
	api = tess_runtime_api();
	if (api->nodes == NULL || api->nodes->abi_version != TESS_NODE_REGISTRY_OPS_ABI_VERSION ||
		api->nodes->struct_size < TESS_NODE_REGISTRY_OPS_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera node registry")));
	api->nodes->add(&parquet_node);
	tess_source_registry_check(api->sources)->add(&parquet_source);
	registered = true;
}

void
_PG_init(void)
{
	RegisterCustomScanMethods(&scan_methods);
	parquet_register();
}

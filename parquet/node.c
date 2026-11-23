/*
 * The module's batch source and its node, TessParquetScan. The source
 * reads every table of the wrapper the scan planner offers it, and checks
 * the clauses on integers, dates and times, IN and the tests of NULL on
 * the files' stored values; the node reads a group in two passes: first
 * the columns of those clauses, every row, into the group's selection,
 * then batches of up to 64 selected rows of one window, making the
 * PostgreSQL values of a column only when a consumer asks for it, for the
 * rows it asks for, keeping them until the next batch. Each column is read
 * by one reader a group, forward window by window, or, for a window
 * behind, for the rows asked alone.
 */
#include "postgres.h"

#include "access/parallel.h"
#include "access/stratnum.h"
#include "access/sysattr.h"
#include "catalog/pg_type.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "commands/explain_state.h"
#include "executor/executor.h"
#include "fmgr.h"
#include "foreign/fdwapi.h"
#include "miscadmin.h"
#include "nodes/extensible.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "port/pg_bitutils.h"
#include "utils/array.h"
#include "utils/date.h"
#include "utils/fmgroids.h"
#include "utils/injection_point.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/rel.h"
#include "utils/ruleutils.h"
#include "utils/timestamp.h"
#include "utils/typcache.h"

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

/*
 * A column of the batch in the bytes form, as consumers asked it: of each
 * row, its NULL flag, the place of its bytes in a window's buffers, their
 * count and its index; the dictionary's count of values and identity, 0
 * where the column has none or two asks named two dictionaries; and the
 * rows made.
 */
typedef struct BatchBytes
{
	bool	   *isnull;
	const char **data;
	uint32	   *lengths;
	int32	   *indexes;
	uint32		nvalues;
	uint64		dictionary;
	uint64		made;
} BatchBytes;

/*
 * The references to windows' arrays that the bytes of a batch point into,
 * freed when the batch's memory is reset: a place is made for a reference
 * before the reader is asked for it, so that none is left behind.
 */
typedef struct BatchHolds
{
	TessParquetHold **holds;
	int			n;
	int			capacity;
	MemoryContextCallback callback;
} BatchHolds;

/* The batch the node publishes: selected rows of one window of a group. */
typedef struct ParquetBatch
{
	TessBatch	batch;
	ParquetNode *node;
	uint64		window;			/* its window's first row in the group */
	int			first;			/* its first row's rank among the window's selected */
	uint16		positions[BATCH_ROWS];	/* of each row, its place in the window */
	bool		contiguous;		/* its rows follow each other from positions[0],
								 * the only place then written */
	int			nrows;
	uint64		mask;			/* its rows selected, which a consumer narrows */
	uint64		all;			/* every row of it, BATCH_ROWS at most */
	Datum	  **values;			/* of each column of the relation asked */
	bool	  **isnull;
	uint64	   *made;			/* of each column, the rows made */
	BatchBytes **bytes;			/* of each column asked in the bytes form, NULL
								 * until the batch's first ask of the form */
} ParquetBatch;

/* What the node keeps of a column it reads: the window its reader holds. */
typedef struct NodeColumn
{
	uint64		window;			/* the window's first row, or NO_WINDOW */
	uint64		forward_end;	/* where the forward reads ended, or NO_WINDOW */
	uint64		held[WINDOW_WORDS]; /* the window's rows the read holds */
	size_t		held_count;		/* how many */
	bool		held_selected;	/* held is the window's selection, read forward */
	uint64		values[PARQUET_WINDOW_ROWS];	/* of the rows held, in order */
	uint64		nulls[WINDOW_WORDS];
	/*
	 * The bytes form of the rows held, as the reader gave it last; the
	 * reader's number of the dictionary it named and the identity the node
	 * took for it, both 0 at each new group, whose readers number anew.
	 */
	const uint8_t *bytes_data[PARQUET_WINDOW_ROWS];
	uint32		bytes_lengths[PARQUET_WINDOW_ROWS];
	int32		bytes_indexes[PARQUET_WINDOW_ROWS];
	uint64		bytes_nulls[WINDOW_WORDS];
	uint64		number;
	uint64		identity;
} NodeColumn;

/*
 * A clause the source checks: the column, the test's kind
 * (TESS_PARQUET_TEST_*), and the side without columns whose value the test
 * takes, of the type type: one value, an array of them for IN, none for
 * the tests of NULL, a string for the tests of strings.
 */
typedef struct ParquetClause
{
	AttrNumber	attno;
	int			kind;
	Expr	   *side;
	Oid			type;
} ParquetClause;

/* A clause as the node checks it. */
typedef struct NodeClause
{
	ParquetClause shape;
	int			column;			/* the scan's column */
	ExprState  *side;
	int64	   *values;			/* computed when first asked for rows */
	bool	   *nulls;
	int			nvalues;
	char	   *bytes;			/* of a test of strings, its string's bytes */
	size_t		nbytes;
	TessParquetTest *test;		/* for the file read, or NULL */
} NodeClause;

/* Whether a test's kind is one of strings, which takes bytes. */
#define STRING_TEST(kind) ((kind) >= TESS_PARQUET_TEST_BYTES_EQ)

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
	BatchHolds *holds;			/* in batch_cxt, NULL after each reset */
	ParquetBatch batch;
	/* The clauses checked, their columns' order, and their sides' values. */
	NodeClause *clauses;
	int			nclauses;
	int		   *clause_order;	/* the clauses by their column */
	bool		values_ready;
	MemoryContext values_cxt;	/* the sides' values, reset at a rescan */
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
static Size node_estimate_dsm(CustomScanState *css, ParallelContext *pcxt);
static void node_initialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate);
static void node_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate);
static void node_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate);
static void node_shutdown(CustomScanState *css);
static void batch_column(TessBatch *batch, int column, const TessRowMask *rows,
						 TessColumnPurpose purpose, TessDatumColumn *result);
static bool batch_bytes(TessBatch *batch, int column, const TessRowMask *rows,
						TessBytesColumn *result);
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
	.EstimateDSMCustomScan = node_estimate_dsm,
	.InitializeDSMCustomScan = node_initialize_dsm,
	.ReInitializeDSMCustomScan = node_reinitialize_dsm,
	.InitializeWorkerCustomScan = node_initialize_worker,
	.ShutdownCustomScan = node_shutdown,
};

static const TessBatchOps batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = batch_column,
	.get_bytes_column = batch_bytes,
};

/* ---- the clauses the source checks ---- */

/* A column of the relation, as a clause names it. */
static bool
column_var(Node *node, Index relid, Var **var)
{
	if (node == NULL || !IsA(node, Var))
		return false;
	*var = (Var *) node;
	return (*var)->varno == relid && (*var)->varlevelsup == 0 && (*var)->varattno > 0;
}

/* The strategy numbers the reader takes are PostgreSQL's. */
StaticAssertDecl(BTLessStrategyNumber == 1 && BTLessEqualStrategyNumber == 2 &&
				 BTEqualStrategyNumber == 3 && BTGreaterEqualStrategyNumber == 4 &&
				 BTGreaterStrategyNumber == 5,
				 "B-tree strategy numbers");

/*
 * The test kind of operator opno between a column of type column and a
 * side of type side, on the left where commuted; -1 for a clause the
 * source does not check. The catalog gives the operator's strategy in the
 * column type's B-tree family, or that of its negator; the reader says
 * what they ask.
 */
static int
comparison_kind(Oid opno, Oid column, Oid side, bool commuted)
{
	Oid			family = lookup_type_cache(column, TYPECACHE_BTREE_OPFAMILY)->btree_opf;
	int			strategy = 0;
	int			negator = 0;

	if (OidIsValid(family))
	{
		strategy = get_op_opfamily_strategy(opno, family);
		if (strategy == 0 && OidIsValid(get_negator(opno)))
			negator = get_op_opfamily_strategy(get_negator(opno), family);
	}
	return tess_parquet_comparison(column, side, strategy, negator, commuted);
}

/* A column of text or varchar, through the relabeling to text that a
 * varchar's operators take. */
static bool
string_var(Node *node, Index relid, Var **var)
{
	if (node != NULL && IsA(node, RelabelType))
		node = (Node *) ((RelabelType *) node)->arg;
	return column_var(node, relid, var) &&
		((*var)->vartype == TEXTOID || (*var)->vartype == VARCHAROID);
}

/*
 * The test kind of a clause on a string column's bytes, -1 for another:
 * = and <> of text, either side the column, and LIKE and NOT LIKE, the
 * column on the left and on the right a constant pattern the reader
 * takes, of fixed parts and %. Under a deterministic collation alone,
 * under which strings are equal only when their bytes are and LIKE of
 * such a pattern matches bytes. side gets the other side.
 */
static int
string_kind(OpExpr *op, Index relid, Var **var, Node **side)
{
	Node	   *left = linitial(op->args);
	Node	   *right = lsecond(op->args);
	Oid			func = get_opcode(op->opno);
	bool		like = func == F_TEXTLIKE || func == F_TEXTNLIKE;

	if ((func != F_TEXTEQ && func != F_TEXTNE && !like) ||
		!OidIsValid(op->inputcollid) || !get_collation_isdeterministic(op->inputcollid))
		return -1;
	if (!string_var(left, relid, var))
	{
		if (like || !string_var(right, relid, var))
			return -1;
		right = left;
	}
	if (contain_var_clause(right))
		return -1;
	/* A NULL pattern folds away before planning: none is taken here. */
	if (like)
	{
		Const	   *pattern = (Const *) right;
		text	   *bytes;

		if (!IsA(right, Const) || pattern->constisnull)
			return -1;
		bytes = DatumGetTextPP(pattern->constvalue);
		if (!tess_parquet_like_checked((const uint8 *) VARDATA_ANY(bytes),
									   VARSIZE_ANY_EXHDR(bytes)))
			return -1;
	}
	*side = right;
	if (func == F_TEXTEQ)
		return TESS_PARQUET_TEST_BYTES_EQ;
	if (func == F_TEXTNE)
		return TESS_PARQUET_TEST_BYTES_NE;
	return func == F_TEXTLIKE ? TESS_PARQUET_TEST_LIKE : TESS_PARQUET_TEST_NOT_LIKE;
}

/*
 * The shape of a clause the source checks: a comparison of a column of an
 * integer type, date, timestamp or timestamptz with a side without the
 * relation's columns, by its type's B-tree family or <>; such a column =
 * ANY an array, IN a list; IS NULL or IS NOT NULL of any column; = and <>
 * of a text or varchar column with a string, and LIKE and NOT LIKE of it
 * with a constant pattern, as string_kind takes them. False for any other
 * clause.
 */
static bool
clause_shape(Expr *clause, Index relid, ParquetClause *shape)
{
	Var		   *var;

	if (IsA(clause, NullTest))
	{
		NullTest   *test = (NullTest *) clause;

		if (test->argisrow || !column_var((Node *) test->arg, relid, &var))
			return false;
		shape->attno = var->varattno;
		shape->kind = test->nulltesttype == IS_NULL ? TESS_PARQUET_TEST_IS_NULL :
			TESS_PARQUET_TEST_IS_NOT_NULL;
		shape->side = NULL;
		shape->type = InvalidOid;
		return true;
	}
	if (IsA(clause, OpExpr) && list_length(((OpExpr *) clause)->args) == 2)
	{
		OpExpr	   *op = (OpExpr *) clause;
		Node	   *left = linitial(op->args);
		Node	   *right = lsecond(op->args);
		bool		commuted = false;
		int			kind;
		Node	   *side;

		kind = string_kind(op, relid, &var, &side);
		if (kind >= 0)
		{
			shape->attno = var->varattno;
			shape->kind = kind;
			shape->side = (Expr *) side;
			shape->type = exprType(side);
			return true;
		}
		if (!column_var(left, relid, &var))
		{
			Node	   *swap = left;

			left = right;
			right = swap;
			commuted = true;
		}
		if (!column_var(left, relid, &var) || contain_var_clause(right))
			return false;
		kind = comparison_kind(op->opno, var->vartype, exprType(right), commuted);
		if (kind < 0)
			return false;
		shape->attno = var->varattno;
		shape->kind = kind;
		shape->side = (Expr *) right;
		shape->type = exprType(right);
		return true;
	}
	if (IsA(clause, ScalarArrayOpExpr))
	{
		ScalarArrayOpExpr *saop = (ScalarArrayOpExpr *) clause;
		Node	   *array = lsecond(saop->args);
		Oid			element = get_element_type(exprType(array));

		if (!saop->useOr || list_length(saop->args) != 2 ||
			!column_var(linitial(saop->args), relid, &var) || contain_var_clause(array) ||
			comparison_kind(saop->opno, var->vartype, element, false) != TESS_PARQUET_TEST_EQ)
			return false;
		shape->attno = var->varattno;
		shape->kind = TESS_PARQUET_TEST_IN;
		shape->side = (Expr *) array;
		shape->type = element;
		return true;
	}
	return false;
}

/* ---- the source ---- */

/*
 * The node's path for the wrapper's serial or partial scan: the plan
 * carries what planning found, and the node checks exactly each clause it
 * can, which it holds as its path's expressions.
 */
static CustomPath *
scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	TessPlanWriter *writer;
	CustomPath *scan;
	Bitmapset  *attrs = NULL;
	Bitmapset  *checked = NULL;
	List	   *expressions = NIL;
	List	   *exact = NIL;
	int			first;

	if (!IsA(path, ForeignPath) || path->parent->fdwroutine == NULL ||
		path->parent->fdwroutine->GetForeignPaths != parquet_get_foreign_paths)
		return NULL;
	/* The projection reads columns alone: a whole row or a system column is
	 * the wrapper's to read. */
	pull_varattnos((Node *) path->pathtarget->exprs, path->parent->relid, &attrs);
	first = bms_next_member(attrs, -1);
	if (first >= 0 && first + FirstLowInvalidHeapAttributeNumber <= 0)
		return NULL;
	for (int i = 0; i < clauses->nclauses; i++)
	{
		ParquetClause shape;

		if (!clauses->usable[i] ||
			!clause_shape(clauses->clauses[i]->clause, path->parent->relid, &shape))
			continue;
		clauses->answers[i] = TESS_CLAUSE_EXACT;
		expressions = lappend(expressions, clauses->clauses[i]->clause);
		exact = lappend(exact, clauses->clauses[i]);
		checked = bms_add_member(checked, shape.attno);
	}
	writer = tess_plan_writer_create(DATA_KIND, DATA_VERSION);
	tess_plan_write_list(writer, "planned", parquet_planned_list(path->parent->fdw_private));
	config.template_path = path;
	config.methods = &path_methods;
	config.node = &parquet_node;
	config.expressions = expressions;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	scan = tess_path_create(&config);
	parquet_source_costs(root, path, exact, bms_num_members(checked), &scan->path.rows,
						 &scan->path.startup_cost, &scan->path.total_cost);
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
	node->column_of = palloc_array(int, Max(natts, 1));
	for (int a = 0; a < natts; a++)
		node->column_of[a] = -1;
	attnums = lsecond(node->planned);
	foreach_int(attnum, attnums)
		node->column_of[attnum - 1] = c++;
	node->columns = palloc0_array(NodeColumn, Max(list_length(attnums), 1));
	node->nclauses = list_length(cscan->custom_exprs);
	node->clauses = palloc0_array(NodeClause, Max(node->nclauses, 1));
	node->clause_order = palloc_array(int, Max(node->nclauses, 1));
	node->values_cxt = AllocSetContextCreate(estate->es_query_cxt, "TessParquetScan values",
											 ALLOCSET_SMALL_SIZES);
	c = 0;
	foreach_ptr(Expr, clause, cscan->custom_exprs)
	{
		NodeClause *checked = &node->clauses[c];

		if (!clause_shape(clause, cscan->scan.scanrelid, &checked->shape) ||
			node->column_of[checked->shape.attno - 1] < 0)
			elog(ERROR, "TessParquetScan cannot check a clause of its plan");
		checked->column = node->column_of[checked->shape.attno - 1];
		if (checked->shape.side != NULL)
			checked->side = ExecInitExpr(checked->shape.side, &css->ss.ps);
		node->clause_order[c] = c;
		c++;
	}
	/* By column, so that a pass reads each column once a window. */
	for (int i = 1; i < node->nclauses; i++)
		for (int j = i; j > 0 &&
			 node->clauses[node->clause_order[j]].column <
			 node->clauses[node->clause_order[j - 1]].column; j--)
		{
			int			swap = node->clause_order[j];

			node->clause_order[j] = node->clause_order[j - 1];
			node->clause_order[j - 1] = swap;
		}
	if (eflags & EXEC_FLAG_EXPLAIN_ONLY)
		return;
	/* A worker reads the leader's list, which it is given after its begin. */
	if (IsParallelWorker() && css->ss.ps.plan->parallel_aware)
		return;
	node->scan = parquet_scan_begin(RelationGetRelid(rel), parquet_strings(linitial(node->planned)),
									attnums);
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

/* Row i's place in the window. */
static inline int
position_of(const ParquetBatch *b, int i)
{
	return b->contiguous ? b->positions[0] + i : b->positions[i];
}

/* The selected rows of the window from its row r on, as a scan's selection says. */
static int
selected_before(const ParquetScan *scan, uint64 window, int r)
{
	if (scan->selection == NULL)
		return r;
	return rank_of(scan->selection + window / 64, r);
}

/*
 * The batch's window held by the reader of the scan's column `c` for the
 * rows `need` of the batch: read where the reader does not hold those rows,
 * forward where the window starts at the last forward one's end or after
 * it, or is the first, and for those rows alone behind it. Writes into
 * `asked` the mask of those rows among the rows held and gives the words
 * of a mask over the rows held.
 */
static pg_always_inline int
window_asked(ParquetNode *node, ParquetBatch *b, int c, uint64 need, uint64 *asked)
{
	ParquetScan *scan = node->scan;
	NodeColumn *col = &node->columns[c];
	uint64		window = b->window;
	size_t		len = (size_t) Min((uint64) PARQUET_WINDOW_ROWS,
								   parquet_scan_group_rows(scan) - window);
	uint64		rows[WINDOW_WORDS] = {0};
	TessParquetReader *reader = parquet_scan_reader(scan, c);
	TessParquetStatus status;
	size_t		count;
	int			base = b->first;

	if (b->contiguous)
	{
		int			from = b->positions[0];

		rows[from / 64] |= need << (from % 64);
		if (from % 64 != 0 && from / 64 + 1 < WINDOW_WORDS)
			rows[from / 64 + 1] |= need >> (64 - from % 64);
	}
	else
	{
		for (uint64 bits = need; bits != 0; bits &= bits - 1)
		{
			int			p = b->positions[pg_rightmost_one_pos64(bits)];

			rows[p / 64] |= UINT64CONST(1) << (p % 64);
		}
	}
	if (col->window != window || !all_held(rows, col->held))
	{
		/* As the reader decides: a window after a gap is read forward. */
		bool		forward = col->forward_end == NO_WINDOW || col->forward_end <= window;

		memset(col->held, 0, sizeof(col->held));
		if (!tess_parquet_read(reader, window, len, rows, (len + 63) / 64, col->held,
							   (len + 63) / 64, &count, &status))
			parquet_raise(&status);
		col->window = window;
		col->held_count = count;
		col->held_selected = forward;
		if (forward)
		{
			col->forward_end = window + len;
			/* A forward read holds every selected row of the window. */
			if (count != (size_t) selected_before(scan, window, (int) len))
				ereport(ERROR,
						(errcode(ERRCODE_DATA_CORRUPTED),
						 errmsg("Parquet column holds fewer rows than its group")));
		}
		INJECTION_POINT("tessera-parquet-window", NULL);
	}
	count = col->held_count;

	/*
	 * A row's place among the rows held: where the read holds the window's
	 * selection, as a forward read does, the batch's rows are consecutive
	 * among them from the batch's first rank; otherwise, behind, its rank
	 * among the rows held.
	 */
	if (col->held_selected)
	{
		asked[base / 64] |= need << (base % 64);
		if (base % 64 != 0 && base / 64 + 1 < WINDOW_WORDS)
			asked[base / 64 + 1] |= need >> (64 - base % 64);
	}
	else
	{
		for (uint64 bits = need; bits != 0; bits &= bits - 1)
		{
			int			p = rank_of(col->held, position_of(b, pg_rightmost_one_pos64(bits)));

			asked[p / 64] |= UINT64CONST(1) << (p % 64);
		}
	}
	return (int) Max((count + 63) / 64, 1);
}

/* The place among the rows held of the batch's row `i`. */
static inline int
held_place(const ParquetBatch *b, const NodeColumn *col, int i)
{
	return col->held_selected ? b->first + i : rank_of(col->held, position_of(b, i));
}

/*
 * Makes the values of the rows `need` of the batch, of its column `column`,
 * the scan's column `c`, from its window held.
 */
static void
make_values(ParquetNode *node, ParquetBatch *b, int column, int c, uint64 need)
{
	ParquetScan *scan = node->scan;
	NodeColumn *col = &node->columns[c];
	uint64		asked[WINDOW_WORDS] = {0};
	int			held_words = window_asked(node, b, c, need, asked);
	TessParquetReader *reader = parquet_scan_reader(scan, c);
	TessParquetStatus status;
	size_t		count = col->held_count;
	int			base = b->first;

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
	if (col->held_selected && need == b->all)
	{
		/* Every row of the batch, in its places: a loop without a mask. */
		for (int i = 0; i < b->nrows; i++)
		{
			bool		null = (col->nulls[(base + i) / 64] >> ((base + i) % 64)) & 1;

			b->isnull[column][i] = null;
			b->values[column][i] = null ? (Datum) 0 : (Datum) col->values[base + i];
		}
	}
	else
	{
		for (uint64 bits = need; bits != 0; bits &= bits - 1)
		{
			int			i = pg_rightmost_one_pos64(bits);
			int			p = col->held_selected ? base + i :
				rank_of(col->held, position_of(b, i));
			bool		null = (col->nulls[p / 64] >> (p % 64)) & 1;

			b->isnull[column][i] = null;
			b->values[column][i] = null ? (Datum) 0 : (Datum) col->values[p];
		}
	}
	b->made[column] |= need;
	scan->own[PARQUET_VALUES_DECODED] += pg_popcount64(need);
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

/* Frees the references of a batch's bytes, at its memory's reset. */
static void
free_holds(void *arg)
{
	BatchHolds *holds = arg;

	for (int i = 0; i < holds->n; i++)
		tess_parquet_hold_free(holds->holds[i]);
	holds->n = 0;
}

/*
 * A place for one more reference of the batch's bytes, made before the
 * reader is asked for it, with the freeing of every reference of the
 * batch arranged in its memory context: a reference the reader writes
 * there is freed with the batch, after an error too.
 */
static TessParquetHold **
next_hold(ParquetNode *node)
{
	BatchHolds *holds = node->holds;

	if (holds == NULL)
	{
		holds = MemoryContextAllocZero(node->batch_cxt, sizeof(BatchHolds));
		holds->capacity = 8;
		holds->holds = MemoryContextAllocZero(node->batch_cxt,
											  sizeof(TessParquetHold *) * holds->capacity);
		holds->callback.func = free_holds;
		holds->callback.arg = holds;
		MemoryContextRegisterResetCallback(node->batch_cxt, &holds->callback);
		node->holds = holds;
	}
	if (holds->n == holds->capacity)
	{
		holds->holds = repalloc0_array(holds->holds, TessParquetHold *, holds->capacity,
									   holds->capacity * 2);
		holds->capacity *= 2;
	}
	holds->holds[holds->n] = NULL;
	return &holds->holds[holds->n++];
}

/*
 * Makes the bytes form of the rows `need` of the batch, of its column
 * `column`, the scan's column `c`, from its window held: each row's place
 * of its bytes, their count and, through a dictionary, its index, with a
 * reference that keeps the window's array for the batch. A dictionary gets
 * an identity of Tessera's API for each new number the reader gives it; a
 * second ask of the batch that names another dictionary leaves the column
 * without one.
 */
static void
make_bytes(ParquetNode *node, ParquetBatch *b, int column, int c, uint64 need)
{
	ParquetScan *scan = node->scan;
	NodeColumn *col = &node->columns[c];
	BatchBytes *view = b->bytes[column];
	uint64		asked[WINDOW_WORDS] = {0};
	int			held_words = window_asked(node, b, c, need, asked);
	TessParquetReader *reader = parquet_scan_reader(scan, c);
	TessParquetBytes out = {0};
	TessParquetHold **hold = next_hold(node);
	TessParquetStatus status;
	uint64		identity = 0;

	out.data = col->bytes_data;
	out.lengths = col->bytes_lengths;
	out.indexes = col->bytes_indexes;
	out.len = PARQUET_WINDOW_ROWS;
	if (!tess_parquet_bytes(reader, asked, held_words, col->bytes_nulls, held_words, &out, hold,
							&status))
		parquet_raise(&status);
	if (out.number != 0)
	{
		if (out.number != col->number)
		{
			col->number = out.number;
			col->identity = tess_dictionary_identity(tess_runtime_api());
		}
		identity = col->identity;
	}
	if (view->made == 0)
	{
		view->dictionary = identity;
		view->nvalues = identity != 0 ? out.nvalues : 0;
	}
	else if (view->dictionary != identity)
	{
		view->dictionary = 0;
		view->nvalues = 0;
	}
	for (uint64 bits = need; bits != 0; bits &= bits - 1)
	{
		int			i = pg_rightmost_one_pos64(bits);
		int			p = held_place(b, col, i);
		bool		null = (col->bytes_nulls[p / 64] >> (p % 64)) & 1;

		view->isnull[i] = null;
		if (null)
			continue;
		view->data[i] = (const char *) col->bytes_data[p];
		view->lengths[i] = col->bytes_lengths[p];
		view->indexes[i] = out.number != 0 ? col->bytes_indexes[p] : 0;
	}
	view->made |= need;
#ifdef USE_ASSERT_CHECKING
	{
		/*
		 * Each row given is the value the plan makes of it, read again row
		 * by row in a scratch arena: its NULL flag and its bytes, in the
		 * batch's place. Values Decoded counts none of them. The arena of
		 * all of them may pass the limit of one value.
		 */
		uint64		nulls[WINDOW_WORDS] = {0};
		uint64	   *values = MemoryContextAllocZero(node->batch_cxt,
													sizeof(uint64) * Max(col->held_count, 1));
		size_t		bytes = 0;
		uint8	   *arena;

		if (!tess_parquet_text_size(reader, asked, held_words, nulls, held_words, &bytes, &status))
			elog(ERROR, "the bytes form of a window passed a row its plan fails: %s",
				 status.message);
		arena = MemoryContextAllocExtended(node->batch_cxt, Max(bytes, 1), MCXT_ALLOC_HUGE);
		if (!tess_parquet_text(reader, arena, bytes, values, Max(col->held_count, 1), &status))
			parquet_raise(&status);
		for (uint64 bits = need; bits != 0; bits &= bits - 1)
		{
			int			i = pg_rightmost_one_pos64(bits);
			int			p = held_place(b, col, i);
			bool		null = (nulls[p / 64] >> (p % 64)) & 1;
			struct varlena *value;

			if (view->isnull[i] != null)
				elog(ERROR, "the bytes form of the batch's row %d differs from its value", i);
			if (null)
				continue;
			value = (struct varlena *) DatumGetPointer((Datum) values[p]);
			if (VARSIZE_ANY_EXHDR(value) != view->lengths[i] ||
				memcmp(VARDATA_ANY(value), view->data[i], view->lengths[i]) != 0)
				elog(ERROR, "the bytes form of the batch's row %d differs from its value", i);
		}
		pfree(arena);
		pfree(values);
	}
#endif
}

/*
 * A text, varchar or bytea column of the batch in the bytes form, made for
 * the rows asked not made yet; false for a column of another type, which
 * the consumer reads through batch_column, and for a structure of a size
 * below the form's.
 */
static bool
batch_bytes(TessBatch *batch, int column, const TessRowMask *rows, TessBytesColumn *result)
{
	ParquetBatch *b = batch->private_data;
	ParquetNode *node = b->node;
	BatchBytes *view;
	uint64		need;
	int			n = Max(b->nrows, 1);

	if (column < 0 || column >= node->relation.ncolumns || node->column_of[column] < 0)
		elog(ERROR, "TessParquetScan was asked for a column its plan does not read");
	if (result->struct_size < TESS_BYTES_COLUMN_MIN_SIZE ||
		node->scan->info[node->column_of[column]].gives_datums)
		return false;
	if (b->bytes == NULL)
		b->bytes = MemoryContextAllocZero(node->batch_cxt,
										  sizeof(BatchBytes *) * Max(node->relation.ncolumns, 1));
	view = b->bytes[column];
	if (view == NULL)
	{
		view = MemoryContextAllocZero(node->batch_cxt, sizeof(BatchBytes));
		view->isnull = MemoryContextAllocZero(node->batch_cxt, sizeof(bool) * n);
		view->data = MemoryContextAllocZero(node->batch_cxt, sizeof(const char *) * n);
		view->lengths = MemoryContextAllocZero(node->batch_cxt, sizeof(uint32) * n);
		view->indexes = MemoryContextAllocZero(node->batch_cxt, sizeof(int32) * n);
		b->bytes[column] = view;
	}
	/* A row asked is made even when a consumer took it out of the mask. */
	need = rows->bits[0] & b->all & ~view->made;
	if (need != 0)
		make_bytes(node, b, column, node->column_of[column], need);
	result->nrows = b->nrows;
	result->isnull = view->isnull;
	result->data = view->data;
	result->lengths = view->lengths;
	result->indexes = view->dictionary != 0 ? view->indexes : NULL;
	result->nvalues = view->dictionary != 0 ? view->nvalues : 0;
	result->dictionary = view->dictionary;
	return true;
}

/* A value of a side of type type, widened as a test takes it. */
static int64
side_value(Datum value, Oid type)
{
	switch (type)
	{
		case INT2OID:
			return DatumGetInt16(value);
		case INT4OID:
			return DatumGetInt32(value);
		case DATEOID:
			return DatumGetDateADT(value);
		default:
			return DatumGetInt64(value);
	}
}

/*
 * The sides' values, through PostgreSQL's evaluation of expressions: when
 * the node is first asked for rows and after each rescan, so that a
 * parameter, a subquery's value or an outer row's is the current one.
 */
static void
compute_values(ParquetNode *node)
{
	ExprContext *econtext = node->css.ss.ps.ps_ExprContext;

	MemoryContextReset(node->values_cxt);
	for (int k = 0; k < node->nclauses; k++)
	{
		NodeClause *clause = &node->clauses[k];
		Datum		value;
		bool		isnull;
		MemoryContext outer;

		clause->nvalues = 0;
		clause->values = NULL;
		clause->nulls = NULL;
		clause->bytes = NULL;
		clause->nbytes = 0;
		if (clause->side == NULL)
			continue;
		value = ExecEvalExprSwitchContext(clause->side, econtext, &isnull);
		outer = MemoryContextSwitchTo(node->values_cxt);
		if (STRING_TEST(clause->shape.kind))
		{
			/* A copy: the value may live in the context reset below. */
			text	   *string = isnull ? NULL : DatumGetTextPCopy(value);

			clause->nvalues = 1;
			clause->nulls = palloc_object(bool);
			clause->nulls[0] = isnull;
			if (string != NULL)
			{
				clause->bytes = VARDATA(string);
				clause->nbytes = VARSIZE(string) - VARHDRSZ;
			}
		}
		else if (clause->shape.kind != TESS_PARQUET_TEST_IN)
		{
			clause->nvalues = 1;
			clause->values = palloc_object(int64);
			clause->nulls = palloc_object(bool);
			clause->nulls[0] = isnull;
			clause->values[0] = isnull ? 0 : side_value(value, clause->shape.type);
		}
		else if (!isnull)
		{
			ArrayType  *array = DatumGetArrayTypeP(value);
			int16		typlen;
			bool		typbyval;
			char		typalign;
			Datum	   *elements;
			bool	   *nulls;

			get_typlenbyvalalign(clause->shape.type, &typlen, &typbyval, &typalign);
			deconstruct_array(array, clause->shape.type, typlen, typbyval, typalign,
							  &elements, &nulls, &clause->nvalues);
			clause->values = palloc_array(int64, Max(clause->nvalues, 1));
			clause->nulls = nulls;
			for (int i = 0; i < clause->nvalues; i++)
				clause->values[i] = nulls[i] ? 0 : side_value(elements[i], clause->shape.type);
		}
		MemoryContextSwitchTo(outer);
		ResetExprContext(econtext);
	}
	node->values_ready = true;
}

/* Frees the clauses' tests, made for the file read. */
static void
free_tests(void *arg)
{
	ParquetNode *node = arg;

	for (int k = 0; k < node->nclauses; k++)
	{
		tess_parquet_test_free(node->clauses[k].test);
		node->clauses[k].test = NULL;
	}
}

/*
 * The clauses' tests for the file read, made once a file: a file's own
 * column decides the unit the values compare in. They are freed with the
 * file's context.
 */
static void
make_tests(ParquetNode *node)
{
	ParquetScan *scan = node->scan;
	MemoryContextCallback *callback;
	TessParquetStatus status;

	if (node->nclauses == 0 || node->clauses[0].test != NULL)
		return;
	/* The freeing first, so that no test outlives a failure. */
	callback = MemoryContextAllocZero(scan->file_cxt, sizeof(MemoryContextCallback));
	callback->func = free_tests;
	callback->arg = node;
	MemoryContextRegisterResetCallback(scan->file_cxt, callback);
	for (int k = 0; k < node->nclauses; k++)
	{
		NodeClause *clause = &node->clauses[k];
		bool		made;

		if (STRING_TEST(clause->shape.kind))
			made = tess_parquet_bytes_test(scan->mapped[clause->column], clause->shape.kind,
										   (const uint8_t *) clause->bytes, clause->nbytes,
										   clause->nulls[0], &clause->test, &status);
		else
			made = tess_parquet_test(scan->mapped[clause->column], clause->shape.kind,
									 clause->values, clause->nulls, (size_t) clause->nvalues,
									 &clause->test, &status);
		if (!made)
		{
			free_tests(node);
			parquet_raise(&status);
		}
	}
}

/*
 * Whether the footer rules the group started out: a clause the statistics
 * of its column's chunk show no row passes, or else one its dictionary
 * shows it, so that a dictionary is read only where no statistics decide.
 * No data page is read.
 */
static bool
group_ruled_out(ParquetNode *node)
{
	ParquetScan *scan = node->scan;
	TessParquetStatus status;

	make_tests(node);
	for (int round = 0; round < 2; round++)
	{
		for (int k = 0; k < node->nclauses; k++)
		{
			NodeClause *clause = &node->clauses[k];
			bool		out;

			if (!tess_parquet_group_ruled_out(scan->file->file, scan->mapped[clause->column],
											  scan->group, clause->test, round == 1, &out,
											  &status))
				parquet_raise(&status);
			if (out)
				return true;
		}
	}
	return false;
}

/*
 * The first pass over the group read: the columns of the clauses, every
 * row, window by window, each read once a window, the clauses checked on
 * the stored values into the group's selection, no value made. False when
 * no row of the group passes.
 */
static bool
select_rows(ParquetNode *node)
{
	ParquetScan *scan = node->scan;
	uint64		rows = parquet_scan_group_rows(scan);
	MemoryContext pass = AllocSetContextCreate(scan->group_cxt, "TessParquetScan clauses",
											   ALLOCSET_SMALL_SIZES);
	TessParquetReader **readers = MemoryContextAllocZero(pass, sizeof(TessParquetReader *) *
														 scan->ncolumns);
	uint64	   *selection = MemoryContextAllocZero(scan->group_cxt,
												   sizeof(uint64) * Max((rows + 63) / 64, 1));
	TessParquetStatus status;
	bool		any = false;

	make_tests(node);
	for (uint64 window = 0; window < rows; window += PARQUET_WINDOW_ROWS)
	{
		size_t		len = (size_t) Min((uint64) PARQUET_WINDOW_ROWS, rows - window);
		int			words = (int) ((len + 63) / 64);
		uint64		passed[WINDOW_WORDS];
		uint64		held[WINDOW_WORDS];
		uint64		every[WINDOW_WORDS];
		uint64	   *out = selection + window / 64;
		int			read = -1;

		CHECK_FOR_INTERRUPTS();
		for (int w = 0; w < words; w++)
			every[w] = w + 1 < words || len % 64 == 0 ? ~UINT64CONST(0) :
				(UINT64CONST(1) << (len % 64)) - 1;
		memcpy(out, every, sizeof(uint64) * words);
		for (int i = 0; i < node->nclauses; i++)
		{
			NodeClause *clause = &node->clauses[node->clause_order[i]];
			int			c = clause->column;
			uint64		left = 0;
			size_t		count;

			for (int w = 0; w < words; w++)
				left |= out[w];
			if (left == 0)
				break;
			if (read != c)
			{
				if (readers[c] == NULL)
					readers[c] = parquet_scan_every_row_reader(scan, c, pass);
				memset(held, 0, sizeof(held));
				if (!tess_parquet_read(readers[c], window, len, every, words, held, words,
									   &count, &status))
					parquet_raise(&status);
				if (count != len)
					ereport(ERROR,
							(errcode(ERRCODE_DATA_CORRUPTED),
							 errmsg("Parquet column holds fewer rows than its group")));
				read = c;
				INJECTION_POINT("tessera-parquet-window", NULL);
			}
			if (!tess_parquet_window_test(readers[c], clause->test, passed, words, &status))
				parquet_raise(&status);
			for (int w = 0; w < words; w++)
				out[w] &= passed[w];
		}
		for (int w = 0; w < words; w++)
			any |= out[w] != 0;
	}
	MemoryContextDelete(pass);
	parquet_scan_select(scan, selection);
	return any;
}

/*
 * The next batch: up to the batch's rows of the selected rows of one
 * window of the group read, from the next row to publish, or NULL past the
 * last group. A query that reads no column publishes the rows and opens
 * no file and reads no page.
 */
static TessBatch *
next_batch(ParquetNode *node)
{
	ParquetScan *scan = node->scan;
	ParquetBatch *b = &node->batch;
	int			natts = node->relation.ncolumns;
	int			n = 0;
	uint64		window = 0;

	if (node->exhausted)
		return NULL;
	if (!node->values_ready && node->nclauses > 0)
		compute_values(node);
	while (n == 0)
	{
		uint64		rows;
		uint64		end;

		CHECK_FOR_INTERRUPTS();
		if (!scan->group_started || node->next_row >= parquet_scan_group_rows(scan))
		{
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
				node->columns[c].number = 0;
				node->columns[c].identity = 0;
			}
			if (node->nclauses > 0 && group_ruled_out(node))
			{
				parquet_scan_skip_group(scan);
				node->next_row = parquet_scan_group_rows(scan);
			}
			else if (node->nclauses > 0 && !select_rows(node))
				node->next_row = parquet_scan_group_rows(scan);
			continue;
		}
		rows = parquet_scan_group_rows(scan);
		window = node->next_row / PARQUET_WINDOW_ROWS * PARQUET_WINDOW_ROWS;
		end = Min(rows, window + PARQUET_WINDOW_ROWS);
		if (scan->selection == NULL)
		{
			/* Every row: the next ones, in order, by the first's place. */
			n = (int) Min((uint64) node->capacity, end - node->next_row);
			b->positions[0] = (uint16) (node->next_row - window);
			node->next_row += n;
			continue;
		}
		/* The selected rows from the next one on, a word at a time. */
		for (uint64 r = node->next_row; r < end && n < node->capacity;)
		{
			uint64		word = scan->selection != NULL ? scan->selection[r / 64] :
				~UINT64CONST(0);

			word &= ~UINT64CONST(0) << (r % 64);
			if (end - r / 64 * 64 < 64)
				word &= (UINT64CONST(1) << (end - r / 64 * 64)) - 1;
			while (word != 0 && n < node->capacity)
			{
				uint64		at = r / 64 * 64 + pg_rightmost_one_pos64(word);

				b->positions[n++] = (uint16) (at - window);
				node->next_row = at + 1;
				word &= word - 1;
			}
			if (n < node->capacity)
			{
				r = (r / 64 + 1) * 64;
				node->next_row = Min(r, end);
			}
		}
		if (n == 0)
			node->next_row = end;
	}
	MemoryContextReset(node->batch_cxt);
	node->holds = NULL;
	b->window = window;
	b->first = selected_before(scan, window, b->positions[0]);
	b->contiguous = scan->selection == NULL ||
		b->positions[n - 1] - b->positions[0] == n - 1;
	b->nrows = n;
	b->all = b->nrows == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << b->nrows) - 1;
	b->mask = b->all;
	b->values = MemoryContextAllocZero(node->batch_cxt, sizeof(Datum *) * Max(natts, 1));
	b->isnull = MemoryContextAllocZero(node->batch_cxt, sizeof(bool *) * Max(natts, 1));
	b->made = MemoryContextAllocZero(node->batch_cxt, sizeof(uint64) * Max(natts, 1));
	b->bytes = NULL;
	b->batch.rows.nrows = b->nrows;
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
	free_tests(node);
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
	node->holds = NULL;
	if (node->scan != NULL)
		parquet_scan_rescan(node->scan);
	/* The sides' values may change: computed again, and the tests made. */
	free_tests(node);
	node->values_ready = false;
	node->exhausted = false;
	node->next_row = 0;
}

/* A parallel scan: the participants take the groups of the leader's list. */
static Size
node_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	return parquet_scan_estimate(((ParquetNode *) css)->scan, pcxt->nworkers);
}

static void
node_initialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	parquet_scan_share(((ParquetNode *) css)->scan, coordinate, pcxt,
					   css->ss.ps.state->es_query_cxt);
}

static void
node_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	parquet_scan_reshare(((ParquetNode *) css)->scan, pcxt, css->ss.ps.state->es_query_cxt);
}

static void
node_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	ParquetNode *node = (ParquetNode *) css;

	node->scan = parquet_scan_attach(RelationGetRelid(css->ss.ss_currentRelation),
									 lsecond(node->planned), coordinate,
									 css->ss.ps.state->es_query_cxt);
}

static void
node_shutdown(CustomScanState *css)
{
	ParquetNode *node = (ParquetNode *) css;

	if (node->scan != NULL)
		parquet_scan_unshare(node->scan);
}

static void
node_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	ParquetNode *node = (ParquetNode *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);

	if (cscan->custom_exprs != NIL)
	{
		List	   *context = set_deparse_context_plan(es->deparse_cxt, css->ss.ps.plan,
													   ancestors);

		/* Prefixed as TessFilter shows its clauses. */
		ExplainPropertyText("Source Exact",
							deparse_expression((Node *) make_ands_explicit(cscan->custom_exprs),
											   context, es->rtable_size > 1 || es->verbose,
											   false),
							es);
	}
	parquet_explain_files(node->planned, node->scan, es);
	if (es->analyze && node->scan != NULL)
	{
		ExplainPropertyUInteger("Row Groups Skipped", NULL,
								parquet_scan_counts(node->scan)[PARQUET_GROUPS_SKIPPED], es);
		ExplainPropertyUInteger("Values Decoded", NULL,
								parquet_scan_counts(node->scan)[PARQUET_VALUES_DECODED], es);
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

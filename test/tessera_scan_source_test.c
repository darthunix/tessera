/*
 * The test source of the capability source-scan: a foreign data wrapper
 * whose tables generate their rows from their options, a batch node that
 * reads such a table, and sources that return the node's path or fail in
 * the ways the scan planner must refuse.
 *
 * A table of n rows (option rows) has row i for i from 1 to n; a column's
 * value is i, or i % mod with the column option mod, NULL where i % nulls
 * is 0 with the column option nulls, and the integer's text for a text
 * column. Rows come in groups of 64, in the order of i; the participants
 * of a parallel scan take groups in turn from a shared counter.
 */
#include "postgres.h"

#include "access/parallel.h"
#include "access/reloptions.h"
#include "access/stratnum.h"
#include "catalog/pg_opfamily_d.h"
#include "catalog/pg_operator_d.h"
#include "catalog/pg_type_d.h"
#include "commands/defrem.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "foreign/fdwapi.h"
#include "foreign/foreign.h"
#include "funcapi.h"
#include "lib/stringinfo.h"
#include "miscadmin.h"
#include "nodes/extensible.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/clauses.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/paths.h"
#include "optimizer/planmain.h"
#include "optimizer/restrictinfo.h"
#include "parser/parsetree.h"
#include "port/atomics.h"
#include "rewrite/rewriteManip.h"
#include "utils/array.h"
#include "utils/builtins.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/rel.h"
#include "utils/ruleutils.h"
#include "utils/varlena.h"

#include "tessera/bridge.h"
#include "tessera/plan.h"
#include "tessera/planner.h"
#include "tessera/runtime.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

PG_FUNCTION_INFO_V1(tessera_test_scan_fdw_handler);
PG_FUNCTION_INFO_V1(tessera_test_scan_sources);
PG_FUNCTION_INFO_V1(tessera_test_scan_source_calls);
PG_FUNCTION_INFO_V1(tessera_test_scan_source_listed);
PG_FUNCTION_INFO_V1(tessera_test_scan_source_fault);
PG_FUNCTION_INFO_V1(tessera_test_scan_source_asks);

#define GROUP_ROWS 64
#define NODE_NAME "tessera_test.scan_source"
#define DATA_KIND "tessera_test.scan_source"
#define DATA_VERSION 1

/*
 * The bytes form the source's batches give their text columns: none, plain,
 * through a dictionary of the column's values, or through one that holds
 * each value at two indexes.
 */
typedef enum BytesForm
{
	BYTES_NONE,
	BYTES_PLAIN,
	BYTES_DICTIONARY,
	BYTES_TWICE
} BytesForm;

/* The table's options: the wrapper's paths, and what the source does. */
typedef struct TableOptions
{
	int64		rows;
	/* The wrapper's path, and a second one when total2 is given. */
	Cost		startup;
	Cost		total;
	bool		second;
	Cost		startup2;
	Cost		total2;
	/* The wrapper's paths have path keys on the first column. */
	bool		pathkeys;
	/* The wrapper adds a path parameterized by the join clauses. */
	bool		parameterized;
	/* A partial path: 0 none, 1 parallel-aware, 2 not parallel-aware. */
	int			partial;
	int			workers;
	/* The wrapper may run in a worker. */
	bool		parallel_safe;
	/* The source: its rows and costs, -1 for the path offered's. */
	double		source_rows;
	Cost		source_startup;
	Cost		source_total;
	/* Its serial path is not parallel-safe; it declines a partial offer. */
	bool		source_unsafe;
	/* The columns whose comparisons it checks exactly, or uses to skip. */
	List	   *exact;
	List	   *prune;
	List	   *partial_exact;
	List	   *partial_prune;
	/* It compares the values with its columns by code of its own. */
	bool		own;
	/*
	 * The bytes form of its text columns; a new dictionary every so many
	 * batches, or one for the scan at 0; and whether the source reads each
	 * batch it makes as a consumer would, to check the form's promises.
	 */
	BytesForm	bytes;
	int64		dictionary_batches;
	bool		check_bytes;
	/* Off: the source asks identities of a root cut before the operation. */
	bool		bridge_identities;
} TableOptions;

/* How one column's values are made. */
typedef struct ColumnGen
{
	Oid			type;
	int64		mod;
	int64		nulls;
	/* A text column's width: see column_text. */
	int64		width;
} ColumnGen;

/* The rows of a table, taken group by group. */
typedef struct Generator
{
	int			natts;
	ColumnGen  *columns;
	int64		rows;
	/* The next group: shared among the participants, or this one's. */
	pg_atomic_uint64 *shared;
	uint64		next_group;
	/* The group being read: its first and last row, the next row. */
	int64		first;
	int64		last;
	int64		row;
	/* The table's bytes form and its dictionaries, from its options. */
	BytesForm	bytes;
	int64		dictionary_batches;
	bool		check_bytes;
	bool		bridge_identities;
} Generator;

/* The sources, by the position of their name. */
enum
{
	SOURCE_READER,
	SOURCE_CHEAPER,
	SOURCE_DECLINER,
	SOURCE_NONE,
	SOURCE_LEAST,
	SOURCE_NULL_CALLBACK,
	SOURCE_PAST,
	SOURCE_FAULTY,
	SOURCE_ANY,
	NSOURCES
};

static const char *const source_names[NSOURCES] = {
	"reader", "cheaper", "decliner", "none", "least", "null_callback", "past",
	"faulty", "any",
};

static int	source_calls[NSOURCES];
static char *fault = NULL;

static void fdw_paths(PlannerInfo *root, RelOptInfo *baserel, Oid foreigntableid);

/* ---- options and values ---- */

static double
option_number(DefElem *def)
{
	char	   *end;
	double		value = strtod(defGetString(def), &end);

	if (*end != '\0')
		elog(ERROR, "option \"%s\" is not a number", def->defname);
	return value;
}

static List *
option_columns(DefElem *def)
{
	List	   *names = NIL;

	if (!SplitIdentifierString(pstrdup(defGetString(def)), ',', &names))
		elog(ERROR, "option \"%s\" is not a list of columns", def->defname);
	return names;
}

static void
read_options(Oid relid, TableOptions *options)
{
	ForeignTable *table = GetForeignTable(relid);
	bool		partial_exact = false;
	bool		partial_prune = false;

	memset(options, 0, sizeof(*options));
	options->total = -1;
	options->workers = 2;
	options->parallel_safe = true;
	options->source_rows = -1;
	options->source_startup = -1;
	options->source_total = -1;
	options->bridge_identities = true;
	foreach_node(DefElem, def, table->options)
	{
		if (strcmp(def->defname, "rows") == 0)
			options->rows = (int64) option_number(def);
		else if (strcmp(def->defname, "startup") == 0)
			options->startup = option_number(def);
		else if (strcmp(def->defname, "total") == 0)
			options->total = option_number(def);
		else if (strcmp(def->defname, "startup2") == 0)
			options->startup2 = option_number(def);
		else if (strcmp(def->defname, "total2") == 0)
		{
			options->second = true;
			options->total2 = option_number(def);
		}
		else if (strcmp(def->defname, "pathkeys") == 0)
			options->pathkeys = defGetBoolean(def);
		else if (strcmp(def->defname, "parameterized") == 0)
			options->parameterized = defGetBoolean(def);
		else if (strcmp(def->defname, "partial") == 0)
		{
			char	   *value = defGetString(def);

			options->partial = strcmp(value, "unaware") == 0 ? 2 :
				defGetBoolean(def) ? 1 : 0;
		}
		else if (strcmp(def->defname, "workers") == 0)
			options->workers = (int) option_number(def);
		else if (strcmp(def->defname, "parallel_safe") == 0)
			options->parallel_safe = defGetBoolean(def);
		else if (strcmp(def->defname, "source_rows") == 0)
			options->source_rows = option_number(def);
		else if (strcmp(def->defname, "source_startup") == 0)
			options->source_startup = option_number(def);
		else if (strcmp(def->defname, "source_total") == 0)
			options->source_total = option_number(def);
		else if (strcmp(def->defname, "source_unsafe") == 0)
			options->source_unsafe = defGetBoolean(def);
		else if (strcmp(def->defname, "exact") == 0)
			options->exact = option_columns(def);
		else if (strcmp(def->defname, "prune") == 0)
			options->prune = option_columns(def);
		else if (strcmp(def->defname, "partial_exact") == 0)
		{
			partial_exact = true;
			options->partial_exact = option_columns(def);
		}
		else if (strcmp(def->defname, "partial_prune") == 0)
		{
			partial_prune = true;
			options->partial_prune = option_columns(def);
		}
		else if (strcmp(def->defname, "own") == 0)
			options->own = defGetBoolean(def);
		else if (strcmp(def->defname, "bytes") == 0)
		{
			char	   *value = defGetString(def);

			if (strcmp(value, "plain") == 0)
				options->bytes = BYTES_PLAIN;
			else if (strcmp(value, "dictionary") == 0)
				options->bytes = BYTES_DICTIONARY;
			else if (strcmp(value, "twice") == 0)
				options->bytes = BYTES_TWICE;
			else
				elog(ERROR, "unknown bytes form \"%s\"", value);
		}
		else if (strcmp(def->defname, "dictionary_batches") == 0)
			options->dictionary_batches = (int64) option_number(def);
		else if (strcmp(def->defname, "check_bytes") == 0)
			options->check_bytes = defGetBoolean(def);
		else if (strcmp(def->defname, "bridge_identities") == 0)
			options->bridge_identities = defGetBoolean(def);
		else
			elog(ERROR, "unknown option \"%s\"", def->defname);
	}
	if (options->total < 0)
		options->total = options->startup + options->rows;
	/* The partial offer is answered as the serial one unless told apart. */
	if (!partial_exact)
		options->partial_exact = options->exact;
	if (!partial_prune)
		options->partial_prune = options->prune;
}

static void
generator_init(Generator *gen, Relation rel)
{
	TupleDesc	desc = RelationGetDescr(rel);
	TableOptions options;

	read_options(RelationGetRelid(rel), &options);
	gen->natts = desc->natts;
	gen->rows = options.rows;
	gen->columns = palloc0_array(ColumnGen, Max(desc->natts, 1));
	for (int i = 0; i < desc->natts; i++)
	{
		Form_pg_attribute attr = TupleDescAttr(desc, i);
		List	   *column_options = GetForeignColumnOptions(RelationGetRelid(rel), i + 1);

		if (attr->atttypid != INT4OID && attr->atttypid != INT8OID &&
			attr->atttypid != TEXTOID)
			elog(ERROR, "the test source has no values of type %s",
				 format_type_be(attr->atttypid));
		gen->columns[i].type = attr->atttypid;
		foreach_node(DefElem, def, column_options)
		{
			if (strcmp(def->defname, "mod") == 0)
				gen->columns[i].mod = (int64) option_number(def);
			else if (strcmp(def->defname, "nulls") == 0)
				gen->columns[i].nulls = (int64) option_number(def);
			else if (strcmp(def->defname, "width") == 0)
				gen->columns[i].width = (int64) option_number(def);
			else
				elog(ERROR, "unknown column option \"%s\"", def->defname);
		}
	}
	gen->bytes = options.bytes;
	gen->dictionary_batches = options.dictionary_batches;
	gen->check_bytes = options.check_bytes;
	gen->bridge_identities = options.bridge_identities;
	gen->shared = NULL;
	gen->next_group = 0;
	gen->first = 1;
	gen->last = 0;
	gen->row = 1;
}

/* Start again from the first group: this participant's counter. */
static void
generator_rescan(Generator *gen)
{
	gen->next_group = 0;
	gen->first = 1;
	gen->last = 0;
	gen->row = 1;
}

/* The next group to read, or false after the last. */
static bool
generator_next_group(Generator *gen)
{
	uint64		group = gen->shared != NULL ?
		pg_atomic_fetch_add_u64(gen->shared, 1) : gen->next_group++;
	int64		first = (int64) group * GROUP_ROWS + 1;

	if (first > gen->rows)
		return false;
	gen->first = first;
	gen->last = Min(first + GROUP_ROWS - 1, gen->rows);
	gen->row = first;
	return true;
}

/* Column column's integer of row i; false when it is NULL. */
static bool
column_integer(const ColumnGen *column, int64 i, int64 *value)
{
	if (column->nulls > 0 && i % column->nulls == 0)
		return false;
	*value = column->mod > 0 ? i % column->mod : i;
	return true;
}

/*
 * A text column's value: the decimal of value, or with the column's width
 * the empty string for 0 and otherwise the decimal followed by
 * (value * 7919) % width bytes of 'x', so that the values of a column
 * differ in their bytes, the decimal standing for the value, and run past
 * the width.
 */
static text *
column_text(const ColumnGen *column, int64 value)
{
	StringInfoData buf;
	text	   *result;

	initStringInfo(&buf);
	if (column->width <= 0 || value != 0)
		appendStringInfo(&buf, INT64_FORMAT, value);
	if (column->width > 0 && value != 0)
		for (int64 n = value * 7919 % column->width; n > 0; n--)
			appendStringInfoChar(&buf, 'x');
	result = cstring_to_text_with_len(buf.data, buf.len);
	pfree(buf.data);
	return result;
}

/* The value a text column's bytes stand for: their leading decimal. */
static int64
bytes_value(const char *data, uint32 len)
{
	int64		value = 0;

	for (uint32 k = 0; k < len && data[k] >= '0' && data[k] <= '9'; k++)
		value = value * 10 + (data[k] - '0');
	return value;
}

/* Row i into values and isnull, allocating text in the current context. */
static void
generator_row(const Generator *gen, int64 i, Datum *values, bool *isnull)
{
	for (int c = 0; c < gen->natts; c++)
	{
		const ColumnGen *column = &gen->columns[c];
		int64		value;

		isnull[c] = !column_integer(column, i, &value);
		if (isnull[c])
			values[c] = (Datum) 0;
		else if (column->type == INT4OID)
			values[c] = Int32GetDatum((int32) value);
		else if (column->type == INT8OID)
			values[c] = Int64GetDatum(value);
		else
			values[c] = PointerGetDatum(column_text(column, value));
	}
}

/* ---- the clauses the source can check ---- */

/*
 * A comparison of a column of the relation, int4 or int8, with a side
 * without columns, of an integer type, by an operator of the integer
 * btree family: the column, the strategy with the column on the left, and
 * the side. False for any other clause.
 */
static bool
clause_shape(Expr *clause, Index relid, AttrNumber *attno, int *strategy, Expr **side)
{
	OpExpr	   *op;
	Node	   *left;
	Node	   *right;
	Var		   *var;
	bool		commuted = false;
	Oid			side_type;

	if (!IsA(clause, OpExpr) || list_length(((OpExpr *) clause)->args) != 2)
		return false;
	op = (OpExpr *) clause;
	left = linitial(op->args);
	right = lsecond(op->args);
	if (!IsA(left, Var))
	{
		Node	   *swap = left;

		left = right;
		right = swap;
		commuted = true;
	}
	if (!IsA(left, Var))
		return false;
	var = (Var *) left;
	if (var->varno != relid || var->varlevelsup != 0 || var->varattno <= 0 ||
		(var->vartype != INT4OID && var->vartype != INT8OID) ||
		contain_var_clause(right) || contain_volatile_functions(right) ||
		contain_subplans(right))
		return false;
	side_type = exprType(right);
	if (side_type != INT2OID && side_type != INT4OID && side_type != INT8OID)
		return false;
	*strategy = get_op_opfamily_strategy(op->opno, INTEGER_BTREE_FAM_OID);
	if (*strategy == 0)
		return false;
	if (commuted)
		*strategy = BTMaxStrategyNumber + 1 - *strategy;
	*attno = var->varattno;
	*side = (Expr *) right;
	return true;
}

static int64
integer_value(Datum value, Oid type)
{
	if (type == INT2OID)
		return DatumGetInt16(value);
	if (type == INT4OID)
		return DatumGetInt32(value);
	return DatumGetInt64(value);
}

/* Whether value compares with side so, as the operator would. */
static bool
compare(int strategy, int64 value, int64 side)
{
	switch (strategy)
	{
		case BTLessStrategyNumber:
			return value < side;
		case BTLessEqualStrategyNumber:
			return value <= side;
		case BTEqualStrategyNumber:
			return value == side;
		case BTGreaterEqualStrategyNumber:
			return value >= side;
		default:
			return value > side;
	}
}

/* Whether some value between least and greatest compares with side so. */
static bool
group_may_pass(int strategy, int64 least, int64 greatest, int64 side)
{
	switch (strategy)
	{
		case BTLessStrategyNumber:
			return least < side;
		case BTLessEqualStrategyNumber:
			return least <= side;
		case BTEqualStrategyNumber:
			return least <= side && side <= greatest;
		case BTGreaterEqualStrategyNumber:
			return greatest >= side;
		default:
			return greatest > side;
	}
}

/*
 * An executor parameter as a parameter of the statement, which deparsing
 * shows as $n without the plan that sets it.
 */
static Node *
params_as_extern(Node *node, void *context)
{
	if (node == NULL)
		return NULL;
	if (IsA(node, Param) && ((Param *) node)->paramkind == PARAM_EXEC)
	{
		Param	   *param = copyObject((Param *) node);

		param->paramkind = PARAM_EXTERN;
		return (Node *) param;
	}
	return expression_tree_mutator(node, params_as_extern, context);
}

/*
 * The clause as text at planning, where no plan gives the deparser its
 * context: its columns named by the relation's own name, an executor
 * parameter as $n.
 */
static char *
clause_text(Expr *clause, Index relid, Oid reloid)
{
	Node	   *copy = params_as_extern(copyObject((Node *) clause), NULL);

	ChangeVarNodes(copy, relid, 1, 0);
	return deparse_expression(copy, deparse_context_for(get_rel_name(reloid), reloid),
							  false, false);
}

/* ---- the node ---- */

/*
 * What the source's own check saw of a text column given through a
 * dictionary: the identity and the period of the batches it saw it last,
 * the bytes of each index under that identity, where the dictionary is
 * small enough to keep them, and whether an index past the column's values
 * appeared, as the form of two indexes a value gives.
 */
typedef struct CheckedColumn
{
	uint64		identity;
	int64		period;
	/* The bytes of each index, in a context reset when the identity changes. */
	MemoryContext context;
	char	  **seen;
	uint32	   *seen_lengths;
	/* A row not NULL at an odd place, and an index past the values. */
	bool		odd_seen;
	bool		high_seen;
} CheckedColumn;

typedef struct SourceScanState
{
	CustomScanState css;
	TessOutput *output;
	TessLayout	layout;
	TessLayout	relation;
	TessProjection *projection;
	TessBuilder *builder;
	const TessRequest *request;
	int			capacity;
	TupleTableSlot *row_slot;
	MemoryContext row_context;
	Generator	gen;
	bool		exhausted;
	/* The clauses it uses: answer, and the shape of each. */
	int			nclauses;
	int		   *answers;
	AttrNumber *attnos;
	int		   *strategies;
	ExprState **sides;
	Oid		   *side_types;
	/* The sides' values, computed at the first rows of each scan. */
	bool		values_ready;
	int64	   *values;
	bool	   *nulls;
	/* The exact clauses, as PostgreSQL evaluates them. */
	ExprState  *exact_qual;
	bool		own;
	const char *path_tag;
	const char *usable;
	const char *unusable;
	/* A row-wise parent: the batch being served, its next row, its targets' columns. */
	bool		rows;
	TessBatch  *active;
	int			next_row;
	TessDatumColumn *columns;
	/*
	 * The batch given in the bytes form: the builder's batch, inner, behind
	 * operations of its own; the arrays of its bytes, reset at the next
	 * batch; the batches made since the scan began; each column's identity
	 * of its current dictionary and the period of batches they are for;
	 * and, for the source's own check, what it saw of each column.
	 */
	TessBatch	bytes_batch;
	TessBatch  *inner;
	/* The columns the projection computes after the relation's. */
	int			ncomputed;
	MemoryContext bytes_context;
	int64		batch_number;
	uint64	   *identities;
	int64		identity_period;
	struct CheckedColumn *checked;
	/*
	 * The check's own count of the batches of the scan, the period its
	 * count makes past those of earlier scans, and the last period.
	 */
	int64		check_batches;
	int64		check_period_base;
	int64		check_last_period;
} SourceScanState;

static Plan *source_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
						 List *tlist, List *clauses, List *custom_plans);
static Node *source_create_state(CustomScan *cscan);
static void source_begin(CustomScanState *css, EState *estate, int eflags);
static TupleTableSlot *source_exec(CustomScanState *css);
static void source_end(CustomScanState *css);
static void source_rescan(CustomScanState *css);
static void source_explain(CustomScanState *css, List *ancestors, ExplainState *es);
static Size source_estimate_dsm(CustomScanState *css, ParallelContext *pcxt);
static void source_initialize_dsm(CustomScanState *css, ParallelContext *pcxt,
								  void *coordinate);
static void source_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt,
									void *coordinate);
static void source_initialize_worker(CustomScanState *css, shm_toc *toc,
									 void *coordinate);
static void source_shutdown(CustomScanState *css);

static const TessNode scan_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = NODE_NAME,
};

static const CustomPathMethods source_path_methods = {
	.CustomName = "TessTestSource",
	.PlanCustomPath = source_plan,
};

static const CustomPathMethods other_path_methods = {
	.CustomName = "TessTestOther",
	.PlanCustomPath = source_plan,
};

static const CustomScanMethods source_scan_methods = {
	.CustomName = "TessTestSource",
	.CreateCustomScanState = source_create_state,
};

static const CustomExecMethods source_exec_methods = {
	.CustomName = "TessTestSource",
	.BeginCustomScan = source_begin,
	.ExecCustomScan = source_exec,
	.EndCustomScan = source_end,
	.ReScanCustomScan = source_rescan,
	.ExplainCustomScan = source_explain,
	.EstimateDSMCustomScan = source_estimate_dsm,
	.InitializeDSMCustomScan = source_initialize_dsm,
	.ReInitializeDSMCustomScan = source_reinitialize_dsm,
	.InitializeWorkerCustomScan = source_initialize_worker,
	.ShutdownCustomScan = source_shutdown,
};

/*
 * The plan: the clauses the source uses as its expressions, no
 * qualifiers, the relation as its scan tuple; or, at a fault's word, the
 * qualifiers PostgreSQL hands it, or no expressions, or a clause added
 * to the relation's own list after its paths were made, which the list
 * PostgreSQL then hands the filter above shares.
 */
static Plan *
source_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
			List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessLayout	layout = TESS_STRUCT_INITIALIZER(TessLayout);

	tess_path_get_info(best_path, &info);
	layout.ncolumns = rel->max_attr;
	layout.ntargets = rel->max_attr;
	config.methods = &source_scan_methods;
	config.layout_policy = TESS_LAYOUT_PROJECTED;
	config.explicit_layout = &layout;
	config.expressions = info.expressions;
	config.node_data = info.node_data;
	config.scanrelid = rel->relid;
	config.scan_tuple_is_relation = true;
	if (fault != NULL && strcmp(fault, "plan_qual") == 0)
		config.qual = extract_actual_clauses(clauses, false);
	if (fault != NULL && strcmp(fault, "plan_missing") == 0)
		config.expressions = NIL;
	if (fault != NULL && strcmp(fault, "extra_clause") == 0 && rel->baserestrictinfo != NIL)
	{
		Var		   *var = makeVar(rel->relid, 1, INT4OID, -1, InvalidOid, 0);
		NullTest   *test = makeNode(NullTest);

		test->arg = (Expr *) var;
		test->nulltesttype = IS_NOT_NULL;
		test->location = -1;
		/* lappend keeps the list's header, which the filter's caller holds. */
		rel->baserestrictinfo = lappend(rel->baserestrictinfo,
										make_simple_restrictinfo(root, (Expr *) test));
	}
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

static Node *
source_create_state(CustomScan *cscan)
{
	SourceScanState *state = (SourceScanState *)
		newNode(sizeof(SourceScanState), T_CustomScanState);

	state->css.methods = &source_exec_methods;
	return (Node *) state;
}

static void
source_begin(CustomScanState *css, EState *estate, int eflags)
{
	SourceScanState *state = (SourceScanState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	Relation	rel = css->ss.ss_currentRelation;
	TessPlanReader *reader;
	List	   *answers;
	List	   *exact = NIL;
	int			index = 0;

	tess_node_require_forward(eflags, "TessTestSource");
	tess_plan_get_info(cscan, &info);
	if (info.node == NULL || strcmp(info.node->name, NODE_NAME) != 0 ||
		info.nchildren != 0 || rel == NULL || cscan->custom_scan_tlist != NIL)
		elog(ERROR, "TessTestSource received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, DATA_KIND, DATA_VERSION);
	state->path_tag = tess_plan_read_string(reader, "path");
	answers = tess_plan_read_int_list(reader, "answers");
	state->own = tess_plan_read_int(reader, "own") != 0;
	state->usable = tess_plan_read_string(reader, "usable");
	state->unusable = tess_plan_read_string(reader, "unusable");
	tess_plan_reader_finish(reader);
	/* At a fault's word the plan may lack its clauses: they are not used then. */
	state->nclauses = Min(list_length(answers), list_length(cscan->custom_exprs));
	state->answers = palloc0_array(int, Max(state->nclauses, 1));
	state->attnos = palloc0_array(AttrNumber, Max(state->nclauses, 1));
	state->strategies = palloc0_array(int, Max(state->nclauses, 1));
	state->sides = palloc0_array(ExprState *, Max(state->nclauses, 1));
	state->side_types = palloc0_array(Oid, Max(state->nclauses, 1));
	state->values = palloc0_array(int64, Max(state->nclauses, 1));
	state->nulls = palloc0_array(bool, Max(state->nclauses, 1));
	foreach_ptr(Expr, clause, cscan->custom_exprs)
	{
		Expr	   *side;

		if (index >= state->nclauses)
			break;
		state->answers[index] = list_nth_int(answers, index);
		if (!clause_shape(clause, cscan->scan.scanrelid, &state->attnos[index],
						  &state->strategies[index], &side))
			elog(ERROR, "TessTestSource cannot check a clause it answered for");
		state->sides[index] = ExecInitExpr(side, &css->ss.ps);
		state->side_types[index] = exprType((Node *) side);
		if (state->answers[index] == TESS_CLAUSE_EXACT)
			exact = lappend(exact, clause);
		index++;
	}
	state->exact_qual = ExecInitQual(exact, &css->ss.ps);
	state->layout = info.layout;
	state->relation = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	state->relation.ncolumns = RelationGetDescr(rel)->natts;
	state->relation.ntargets = RelationGetDescr(rel)->natts;
	state->row_slot = ExecInitExtraTupleSlot(estate, RelationGetDescr(rel), &TTSOpsVirtual);
	state->ncomputed = list_length(info.computed);
	if (info.computed != NIL)
		state->projection = tess_node_projection(css, css->ss.ss_ScanTupleSlot, &state->relation,
												 state->relation.ncolumns, info.computed);
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot, &info.layout);
	state->row_context = AllocSetContextCreate(estate->es_query_cxt, "TessTestSource row",
											   ALLOCSET_SMALL_SIZES);
	if ((eflags & EXEC_FLAG_EXPLAIN_ONLY) == 0)
		generator_init(&state->gen, rel);
}

/*
 * The sides without columns of the clauses used, through PostgreSQL's
 * evaluation of expressions: at the first rows of each scan, so that a
 * subquery's value is computed when first read, a worker's taken from the
 * gather, and an outer row's the current one.
 */
static void
compute_values(SourceScanState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;

	for (int i = 0; i < state->nclauses; i++)
	{
		Datum		value = ExecEvalExprSwitchContext(state->sides[i], econtext,
													  &state->nulls[i]);

		if (!state->nulls[i])
			state->values[i] = integer_value(value, state->side_types[i]);
	}
	ResetExprContext(econtext);
	state->values_ready = true;
}

/* Whether a clause used to skip rows rejects every row of the group. */
static bool
group_rejected(SourceScanState *state)
{
	Generator  *gen = &state->gen;

	for (int c = 0; c < state->nclauses; c++)
	{
		const ColumnGen *column = &gen->columns[state->attnos[c] - 1];
		bool		any = false;
		int64		least = 0;
		int64		greatest = 0;

		if (state->answers[c] != TESS_CLAUSE_PRUNE_ONLY)
			continue;
		if (state->nulls[c])
			return true;
		for (int64 i = gen->first; i <= gen->last; i++)
		{
			int64		value;

			if (!column_integer(column, i, &value))
				continue;
			least = any ? Min(least, value) : value;
			greatest = any ? Max(greatest, value) : value;
			any = true;
		}
		if (!any || !group_may_pass(state->strategies[c], least, greatest, state->values[c]))
			return true;
	}
	return false;
}

/* Whether row i passes the exact clauses, by the source's own comparisons. */
static bool
row_passes_own(SourceScanState *state, int64 i)
{
	for (int c = 0; c < state->nclauses; c++)
	{
		int64		value;

		if (state->answers[c] != TESS_CLAUSE_EXACT)
			continue;
		if (state->nulls[c] ||
			!column_integer(&state->gen.columns[state->attnos[c] - 1], i, &value) ||
			!compare(state->strategies[c], value, state->values[c]))
			return false;
	}
	return true;
}

/* ---- the bytes form of the source's batches ---- */

/*
 * The asks of each form of the source's batches, and the batches its own
 * check read: in the calling process alone, so that a case that counts
 * them runs serial.
 */
static int64 datum_asks;
static int64 bytes_asks;
static int64 bytes_checks;
static int64 projection_checks;

/* The most indexes of which the check keeps what each held. */
#define CHECK_MAX_VALUES 4096

static void
bytes_datum_column(TessBatch *batch, int column, const TessRowMask *rows,
				   TessColumnPurpose purpose, TessDatumColumn *result)
{
	SourceScanState *state = batch->private_data;

	datum_asks++;
	state->inner->ops->get_datum_column(state->inner, column, rows, purpose, result);
}

static void
bytes_release(TessBatch *batch)
{
	SourceScanState *state = batch->private_data;

	if (TESS_ABI_HAS_FIELD(state->inner->ops, TessBatchOps, release) &&
		state->inner->ops->release != NULL)
		state->inner->ops->release(state->inner);
}

/*
 * The count of values of a text column's dictionary, or 0 where it is given
 * without one: a column of values bounded by mod, in a form through a
 * dictionary, whose indexes fit an int32.
 */
static uint32
dictionary_values(const SourceScanState *state, int column)
{
	const ColumnGen *gen_column = &state->gen.columns[column];
	int64		factor = state->gen.bytes == BYTES_TWICE ? 2 : 1;

	if (state->gen.bytes == BYTES_NONE || state->gen.bytes == BYTES_PLAIN ||
		gen_column->type != TEXTOID || gen_column->mod <= 0 ||
		gen_column->mod > PG_INT32_MAX / factor)
		return 0;
	return (uint32) (gen_column->mod * factor);
}

/*
 * A text column's bytes for the rows asked: the bytes behind the builder's
 * values, which live until its next batch; through a dictionary, of the
 * column's own identity, each row's index is the value its bytes stand
 * for, and in the form of two indexes a value the odd rows' is moved past
 * the column's values. Every field the structure's size covers is written.
 */
static bool
bytes_column(TessBatch *batch, int column, const TessRowMask *rows,
			 TessBytesColumn *result)
{
	SourceScanState *state = batch->private_data;
	TessDatumColumn datums = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	int			nrows = batch->rows.nrows;
	uint32		nvalues;
	bool	   *isnull;
	const char **data;
	uint32	   *lengths;
	int32	   *indexes = NULL;
	MemoryContext old;
	int			row = -1;

	if (result->struct_size < TESS_BYTES_COLUMN_MIN_SIZE ||
		column < 0 || column >= state->gen.natts ||
		state->gen.columns[column].type != TEXTOID)
		return false;
	nvalues = state->identities[column] != 0 ? dictionary_values(state, column) : 0;
	bytes_asks++;
	state->inner->ops->get_datum_column(state->inner, column, rows,
										TESS_COLUMN_FOR_PROJECTION, &datums);
	old = MemoryContextSwitchTo(state->bytes_context);
	isnull = palloc0_array(bool, Max(nrows, 1));
	data = palloc0_array(const char *, Max(nrows, 1));
	lengths = palloc0_array(uint32, Max(nrows, 1));
	if (nvalues > 0)
		indexes = palloc0_array(int32, Max(nrows, 1));
	MemoryContextSwitchTo(old);
	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		Pointer		value = DatumGetPointer(datums.values[row]);

		isnull[row] = datums.isnull[row];
		if (isnull[row])
			continue;
		data[row] = VARDATA_ANY(value);
		lengths[row] = VARSIZE_ANY_EXHDR(value);
		if (nvalues > 0)
			indexes[row] = (int32) (bytes_value(data[row], lengths[row]) +
									(state->gen.bytes == BYTES_TWICE && row % 2 == 1 ?
									 state->gen.columns[column].mod : 0));
	}
	result->nrows = nrows;
	result->isnull = isnull;
	result->data = data;
	result->lengths = lengths;
	result->indexes = indexes;
	result->nvalues = nvalues;
	result->dictionary = nvalues > 0 ? state->identities[column] : 0;
	return true;
}

static const TessBatchOps bytes_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = bytes_datum_column,
	.release = bytes_release,
	.get_bytes_column = bytes_column,
};

/*
 * The count of values the check expects of a text column's dictionary, by
 * its own reading of the table's options and of the bridge: 0 for a plain
 * form, a bridge whose root lacks the identities' operation or a table
 * that asks a cut one, and a column of values an int32 index cannot hold.
 */
static uint32
expected_values(const SourceScanState *state, int column)
{
	const TessApi *api = tess_runtime_api();
	int64		mod = state->gen.columns[column].mod;

	if (!state->gen.bridge_identities ||
		!TESS_ABI_HAS_FIELD(api, TessApi, next_dictionary_identity) ||
		api->next_dictionary_identity == NULL || mod <= 0)
		return 0;
	if (state->gen.bytes == BYTES_DICTIONARY)
		return mod <= PG_INT32_MAX ? (uint32) mod : 0;
	if (state->gen.bytes == BYTES_TWICE)
		return mod <= PG_INT32_MAX / 2 ? (uint32) (2 * mod) : 0;
	return 0;
}

/* A view of the form and copies of what it held when it was given. */
typedef struct HeldView
{
	TessBytesColumn view;
	bool	   *isnull;
	const char **data;
	uint32	   *lengths;
	int32	   *indexes;
	char	  **bytes;
} HeldView;

/* A copy of what view holds for the rows of mask. */
static void
hold_view(HeldView *held, const TessBytesColumn *view, const TessRowMask *mask)
{
	int			nrows = Max(view->nrows, 1);
	int			row = -1;

	held->view = *view;
	held->isnull = palloc0_array(bool, nrows);
	held->data = palloc0_array(const char *, nrows);
	held->lengths = palloc0_array(uint32, nrows);
	held->indexes = palloc0_array(int32, nrows);
	held->bytes = palloc0_array(char *, nrows);
	while ((row = tess_row_mask_next(mask, row)) >= 0)
	{
		held->isnull[row] = view->isnull[row];
		if (view->isnull[row])
			continue;
		held->data[row] = view->data[row];
		held->lengths[row] = view->lengths[row];
		if (view->indexes != NULL)
			held->indexes[row] = view->indexes[row];
		held->bytes[row] = palloc(Max(view->lengths[row], 1));
		memcpy(held->bytes[row], view->data[row], view->lengths[row]);
	}
}

/* Whether the view still holds what it held, and its bytes the Datum's. */
static bool
view_held(const HeldView *held, const TessRowMask *mask, const TessDatumColumn *datums)
{
	const TessBytesColumn *view = &held->view;
	int			row = -1;

	while ((row = tess_row_mask_next(mask, row)) >= 0)
	{
		Pointer		value;

		if (view->isnull[row] != held->isnull[row] ||
			view->isnull[row] != datums->isnull[row])
			return false;
		if (view->isnull[row])
			continue;
		value = DatumGetPointer(datums->values[row]);
		if (view->data[row] != held->data[row] || view->data[row] == NULL ||
			view->lengths[row] != held->lengths[row] ||
			(view->indexes != NULL && view->indexes[row] != held->indexes[row]) ||
			memcmp(view->data[row], held->bytes[row], view->lengths[row]) != 0 ||
			VARSIZE_ANY_EXHDR(value) != view->lengths[row] ||
			memcmp(VARDATA_ANY(value), view->data[row], view->lengths[row]) != 0)
			return false;
	}
	return true;
}

/*
 * The source reads its own batch as a consumer would, and fails the query
 * where the form breaks a promise of the spec of string-bytes. Every text
 * column is asked as bytes first and its view held; then every column as
 * Datum values and every text column as bytes again; and only then is
 * each view checked: its arrays and its bytes as given, the bytes the
 * row's Datum's. Each view's layout, and its dictionary against an oracle
 * of its own: the count of values and each row's index as the generator
 * makes them, one index's bytes the same in every batch of an identity,
 * the identities of two columns apart, a column's identity kept within its
 * period of batches and changed at the next and after a rescan. A result
 * filled with another column's dictionary gets a plain column's fields
 * written; a structure of the size before its last field, allocated at
 * that size, is answered false and left as it was; one allocated at the
 * form's size is answered. The source's own asks are not counted.
 */
static void
check_bytes(SourceScanState *state, TessBatch *batch)
{
	int64		period = state->check_period_base +
		(state->gen.dictionary_batches > 0 ?
		 state->check_batches / state->gen.dictionary_batches : 0);
	int64		saved_datum = datum_asks;
	int64		saved_bytes = bytes_asks;
	int			natts = state->gen.natts;
	HeldView   *held = palloc0_array(HeldView, Max(natts, 1));
	TessBytesColumn dictionary_view = TESS_STRUCT_INITIALIZER(TessBytesColumn);

	for (int c = 0; c < natts; c++)
	{
		TessBytesColumn bytes;
		uint32		expected = expected_values(state, c);
		CheckedColumn *checked = &state->checked[c];
		int			row = -1;

		if (state->gen.columns[c].type != TEXTOID)
			continue;
		if (!tess_batch_bytes_column(batch, c, &batch->rows, &bytes))
			elog(ERROR, "the test source did not give its text column %d as bytes", c);
		if (bytes.struct_size != sizeof(TessBytesColumn) ||
			bytes.nrows != batch->rows.nrows || bytes.nvalues != expected ||
			(bytes.indexes == NULL) != (expected == 0) ||
			(bytes.dictionary == 0) != (expected == 0))
			elog(ERROR, "the test source's bytes of column %d break their layout", c);
		hold_view(&held[c], &bytes, &batch->rows);
		if (expected == 0)
			continue;
		dictionary_view = bytes;
		for (int other = 0; other < c; other++)
			if (held[other].view.dictionary == bytes.dictionary)
				elog(ERROR, "the test source's columns %d and %d share an identity", other, c);
		if (checked->identity != 0 &&
			(period == checked->period) != (bytes.dictionary == checked->identity))
			elog(ERROR, "the test source's dictionary of column %d broke its identity", c);
		if (bytes.dictionary != checked->identity)
		{
			checked->identity = bytes.dictionary;
			if (checked->context == NULL)
				checked->context = AllocSetContextCreate(state->css.ss.ps.state->es_query_cxt,
														 "TessTestSource check",
														 ALLOCSET_DEFAULT_SIZES);
			else
				MemoryContextReset(checked->context);
			checked->seen = NULL;
			if (expected <= CHECK_MAX_VALUES)
			{
				checked->seen = MemoryContextAllocZero(checked->context,
													   sizeof(char *) * expected);
				checked->seen_lengths = MemoryContextAllocZero(checked->context,
															   sizeof(uint32) * expected);
			}
		}
		checked->period = period;
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
		{
			int64		index;

			if (bytes.isnull[row])
				continue;
			index = bytes_value(bytes.data[row], bytes.lengths[row]) +
				(state->gen.bytes == BYTES_TWICE && row % 2 == 1 ? state->gen.columns[c].mod : 0);
			if (bytes.indexes[row] != index)
				elog(ERROR, "the test source's row %d of column %d has another index", row, c);
			if (row % 2 == 1)
				checked->odd_seen = true;
			if (index >= state->gen.columns[c].mod)
				checked->high_seen = true;
			if (checked->seen == NULL)
				continue;
			if (checked->seen[index] == NULL)
			{
				checked->seen[index] = MemoryContextAlloc(checked->context,
														  Max(bytes.lengths[row], 1));
				memcpy(checked->seen[index], bytes.data[row], bytes.lengths[row]);
				checked->seen_lengths[index] = bytes.lengths[row];
			}
			else if (checked->seen_lengths[index] != bytes.lengths[row] ||
					 memcmp(checked->seen[index], bytes.data[row], bytes.lengths[row]) != 0)
				elog(ERROR, "the test source's index %ld of column %d holds two values",
					 (long) index, c);
		}
	}
	/* The asks between: every column's Datums, every text column's bytes. */
	for (int c = 0; c < natts; c++)
	{
		TessDatumColumn datums = TESS_STRUCT_INITIALIZER(TessDatumColumn);
		TessBytesColumn again;

		batch->ops->get_datum_column(batch, c, &batch->rows, TESS_COLUMN_FOR_PROJECTION,
									 &datums);
		if (state->gen.columns[c].type == TEXTOID)
			(void) tess_batch_bytes_column(batch, c, &batch->rows, &again);
	}
	for (int c = 0; c < natts; c++)
	{
		TessDatumColumn datums = TESS_STRUCT_INITIALIZER(TessDatumColumn);

		if (state->gen.columns[c].type != TEXTOID)
			continue;
		batch->ops->get_datum_column(batch, c, &batch->rows, TESS_COLUMN_FOR_PROJECTION,
									 &datums);
		if (!view_held(&held[c], &batch->rows, &datums))
			elog(ERROR, "the test source's bytes of column %d changed", c);
	}
	for (int c = 0; c < natts; c++)
	{
		Size		short_size = offsetof(TessBytesColumn, dictionary);
		TessBytesColumn *short_form;
		TessBytesColumn *exact;
		TessBytesColumn reused;
		char	   *before;

		if (state->gen.columns[c].type != TEXTOID)
			continue;
		if (expected_values(state, c) == 0 && dictionary_view.dictionary != 0)
		{
			reused = dictionary_view;
			if (!batch->ops->get_bytes_column(batch, c, &batch->rows, &reused) ||
				reused.indexes != NULL || reused.nvalues != 0 || reused.dictionary != 0)
				elog(ERROR, "the test source left a dictionary in plain column %d", c);
		}
		short_form = palloc(short_size);
		memset(short_form, 0x5a, short_size);
		short_form->struct_size = short_size;
		before = palloc(short_size);
		memcpy(before, short_form, short_size);
		if (batch->ops->get_bytes_column(batch, c, &batch->rows, short_form) ||
			memcmp(before, short_form, short_size) != 0)
			elog(ERROR, "the test source answered a structure of an older size");
		pfree(short_form);
		pfree(before);
		exact = palloc(TESS_BYTES_COLUMN_MIN_SIZE);
		*exact = (TessBytesColumn) TESS_STRUCT_INITIALIZER(TessBytesColumn);
		if (!batch->ops->get_bytes_column(batch, c, &batch->rows, exact))
			elog(ERROR, "the test source refused a structure of the form's size");
		pfree(exact);
	}
	datum_asks = saved_datum;
	bytes_asks = saved_bytes;
	bytes_checks++;
	state->check_batches++;
	state->check_last_period = period;
}

/*
 * A new identity of a dictionary, from the bridge, or with bridge_identities
 * off from a root cut before the operation, as an older bridge gives it: 0.
 */
static uint64
source_identity(SourceScanState *state)
{
	const TessApi *api = tess_runtime_api();
	Size		cut = Min(TESS_ABI_SIZE_INCLUDING_FIELD(TessApi, kernels),
						  api->struct_size);
	TessApi    *older;
	uint64		identity;

	if (state->gen.bridge_identities)
		return tess_dictionary_identity(api);
	older = palloc(cut);
	memcpy(older, api, cut);
	older->struct_size = cut;
	identity = tess_dictionary_identity(older);
	pfree(older);
	return identity;
}

/*
 * The projection's batch over the source's: each text column it passes on
 * gives the bytes the source's own batch gives, at the same addresses, and
 * each column it computes is not offered. The source's own asks are not
 * counted.
 */
static void
check_projection_bytes(SourceScanState *state, TessBatch *batch)
{
	int64		saved_datum = datum_asks;
	int64		saved_bytes = bytes_asks;
	MemoryContext old = MemoryContextSwitchTo(state->bytes_context);

	for (int c = 0; c < state->relation.ncolumns + state->ncomputed; c++)
	{
		TessBytesColumn through;
		TessBytesColumn own;
		int			row = -1;

		if (c >= state->relation.ncolumns)
		{
			if (tess_batch_bytes_column(batch, c, &batch->rows, &through))
				elog(ERROR, "the projection gave its computed column %d as bytes", c);
			continue;
		}
		if (state->gen.columns[c].type != TEXTOID)
			continue;
		if (!tess_batch_bytes_column(batch, c, &batch->rows, &through) ||
			!tess_batch_bytes_column(&state->bytes_batch, c, &batch->rows, &own))
			elog(ERROR, "the projection did not pass the bytes of column %d on", c);
		while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
			if (through.isnull[row] != own.isnull[row] ||
				(!own.isnull[row] &&
				 (through.data[row] != own.data[row] ||
				  through.lengths[row] != own.lengths[row])))
				elog(ERROR, "the projection changed the bytes of row %d of column %d", row, c);
		if (through.dictionary != own.dictionary || through.nvalues != own.nvalues)
			elog(ERROR, "the projection changed the dictionary of column %d", c);
	}
	MemoryContextSwitchTo(old);
	datum_asks = saved_datum;
	bytes_asks = saved_bytes;
	projection_checks++;
}

/*
 * The builder's batch behind the operations of the bytes form, with its
 * rows: the arrays of the last batch's bytes freed, and each dictionary
 * column's identity taken anew at the start of each period of batches.
 */
static TessBatch *
bytes_wrap(SourceScanState *state, TessBatch *inner)
{
	int64		period = state->gen.dictionary_batches > 0 ?
		state->batch_number / state->gen.dictionary_batches : 0;
	MemoryContext query = state->css.ss.ps.state->es_query_cxt;

	if (state->bytes_context == NULL)
	{
		state->bytes_context = AllocSetContextCreate(query, "TessTestSource bytes",
													 ALLOCSET_DEFAULT_SIZES);
		state->identities = MemoryContextAllocZero(query,
												   sizeof(uint64) * Max(state->gen.natts, 1));
		state->checked = MemoryContextAllocZero(query,
												sizeof(CheckedColumn) * Max(state->gen.natts, 1));
		state->identity_period = -1;
	}
	else
		MemoryContextReset(state->bytes_context);
	state->batch_number++;
	if (period != state->identity_period)
	{
		for (int c = 0; c < state->gen.natts; c++)
			state->identities[c] = dictionary_values(state, c) > 0 ?
				source_identity(state) : 0;
		state->identity_period = period;
	}
	state->inner = inner;
	state->bytes_batch = *inner;
	state->bytes_batch.ops = &bytes_ops;
	state->bytes_batch.private_data = state;
	if (state->gen.check_bytes)
	{
		MemoryContext old = MemoryContextSwitchTo(state->bytes_context);

		check_bytes(state, &state->bytes_batch);
		MemoryContextSwitchTo(old);
	}
	return &state->bytes_batch;
}

/* The next batch of rows that pass the exact clauses, or NULL at the end. */
static TessBatch *
next_batch(SourceScanState *state)
{
	Generator  *gen = &state->gen;
	TupleTableSlot *slot = state->row_slot;
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;
	TessBatch  *batch;

	if (state->exhausted)
		return NULL;
	if (!state->values_ready)
		compute_values(state);
	tess_builder_reset(state->builder);
	while (!tess_builder_is_full(state->builder))
	{
		MemoryContext old;
		bool		pass;

		CHECK_FOR_INTERRUPTS();
		if (gen->row > gen->last)
		{
			if (!generator_next_group(gen))
			{
				state->exhausted = true;
				break;
			}
			if (group_rejected(state))
			{
				gen->row = gen->last + 1;
				continue;
			}
		}
		ExecClearTuple(slot);
		old = MemoryContextSwitchTo(state->row_context);
		generator_row(gen, gen->row, slot->tts_values, slot->tts_isnull);
		MemoryContextSwitchTo(old);
		ExecStoreVirtualTuple(slot);
		if (state->own)
			pass = row_passes_own(state, gen->row);
		else
		{
			econtext->ecxt_scantuple = slot;
			pass = ExecQualAndReset(state->exact_qual, econtext);
		}
		if (pass)
			tess_builder_append_slot(state->builder, slot);
		ExecClearTuple(slot);
		MemoryContextReset(state->row_context);
		gen->row++;
	}
	batch = tess_builder_finish(state->builder,
								RelationGetRelid(state->css.ss.ss_currentRelation));
	if (batch != NULL && state->gen.bytes != BYTES_NONE)
		batch = bytes_wrap(state, batch);
	if (batch != NULL && state->projection != NULL)
	{
		batch = tess_projection_wrap(state->projection, batch);
		if (state->gen.bytes != BYTES_NONE && state->gen.check_bytes)
			check_projection_bytes(state, batch);
	}
	return batch;
}

static void
drop_active(SourceScanState *state)
{
	if (state->active != NULL && state->projection != NULL)
		state->active->ops->release(state->active);
	state->active = NULL;
}

/* A row-wise parent: the next row of the batch being served. */
static TupleTableSlot *
exec_rows(SourceScanState *state)
{
	TupleTableSlot *slot = state->css.ss.ps.ps_ResultTupleSlot;

	for (;;)
	{
		int			row;

		if (state->active == NULL)
		{
			TessBatch  *batch = next_batch(state);

			if (batch == NULL)
				return NULL;
			tess_batch_target_columns(batch, &state->layout, state->layout.ntargets,
									  state->columns);
			state->active = batch;
			state->next_row = tess_row_mask_next(&batch->rows, -1);
		}
		if (state->next_row < 0)
		{
			drop_active(state);
			continue;
		}
		row = state->next_row;
		state->next_row = tess_row_mask_next(&state->active->rows, row);
		ExecClearTuple(slot);
		for (int target = 0; target < state->layout.ntargets; target++)
		{
			slot->tts_values[target] = state->columns[target].values[row];
			slot->tts_isnull[target] = state->columns[target].isnull[row];
		}
		slot->tts_tableOid = state->active->table_oid;
		return ExecStoreVirtualTuple(slot);
	}
}

static TupleTableSlot *
source_exec(CustomScanState *css)
{
	SourceScanState *state = (SourceScanState *) css;
	TessBatch  *batch;

	if (state->request == NULL)
	{
		TessBuilderConfig config = TESS_STRUCT_INITIALIZER(TessBuilderConfig);

		state->request = tess_output_request(state->output);
		state->capacity = state->request->max_batch_rows > 0 ?
			Min(state->request->max_batch_rows, GROUP_ROWS) : GROUP_ROWS;
		state->rows = state->request->output_mode == TESS_OUTPUT_ROWS;
		if (state->rows)
			state->columns = palloc0_array(TessDatumColumn, Max(state->layout.ntargets, 1));
		config.parent_context = css->ss.ps.state->es_query_cxt;
		config.tuple_desc = state->row_slot->tts_tupleDescriptor;
		config.ncolumns = config.tuple_desc->natts;
		config.capacity = state->capacity;
		state->builder = tess_builder_create(&config);
	}
	if (state->rows)
		return exec_rows(state);
	tess_output_release(state->output);
	batch = next_batch(state);
	if (batch == NULL)
		return NULL;
	return tess_output_publish(state->output, batch);
}

static void
source_end(CustomScanState *css)
{
	SourceScanState *state = (SourceScanState *) css;

	drop_active(state);
	tess_output_end(state->output);
	/*
	 * The form of two indexes a value gave an index past the values, where
	 * a row not NULL stood at an odd place.
	 */
	if (state->checked != NULL && state->gen.bytes == BYTES_TWICE)
		for (int c = 0; c < state->gen.natts; c++)
			if (state->checked[c].odd_seen && !state->checked[c].high_seen)
				elog(ERROR, "the test source's column %d gave each value one index", c);
}

/* The next scan reads every group again, and computes the values anew. */
static void
source_rescan(CustomScanState *css)
{
	SourceScanState *state = (SourceScanState *) css;

	tess_output_clear(state->output);
	drop_active(state);
	if (state->projection != NULL)
		tess_projection_reset(state->projection);
	if (state->builder != NULL)
		tess_builder_reset(state->builder);
	generator_rescan(&state->gen);
	state->exhausted = false;
	state->values_ready = false;
	/* A dictionary given again after a rescan gets an identity anew. */
	state->batch_number = 0;
	state->identity_period = -1;
	/* The check's own count: the next scan's first period is a new one. */
	if (state->check_batches > 0)
		state->check_period_base = state->check_last_period + 1;
	state->check_batches = 0;
}

/* The clauses of one answer, as the plan's expressions show them. */
static void
explain_clauses(SourceScanState *state, List *ancestors, ExplainState *es,
				int answer, const char *label)
{
	CustomScan *cscan = castNode(CustomScan, state->css.ss.ps.plan);
	List	   *clauses = NIL;
	List	   *context;
	int			index = 0;

	foreach_ptr(Expr, clause, cscan->custom_exprs)
	{
		if (index < state->nclauses && state->answers[index] == answer)
			clauses = lappend(clauses, clause);
		index++;
	}
	if (clauses == NIL)
		return;
	context = set_deparse_context_plan(es->deparse_cxt, state->css.ss.ps.plan, ancestors);
	/* Prefixed as TessFilter shows its clauses. */
	ExplainPropertyText(label,
						deparse_expression((Node *) make_ands_explicit(clauses), context,
										   es->rtable_size > 1 || es->verbose,
										   false),
						es);
}

static void
source_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	SourceScanState *state = (SourceScanState *) css;

	ExplainPropertyText("Foreign Path", state->path_tag, es);
	explain_clauses(state, ancestors, es, TESS_CLAUSE_EXACT, "Source Exact");
	explain_clauses(state, ancestors, es, TESS_CLAUSE_PRUNE_ONLY, "Source Prune");
	if (state->own)
		ExplainPropertyText("Source Check", "own", es);
	if (es->verbose && state->usable[0] != '\0')
		ExplainPropertyText("Offered Usable", state->usable, es);
	if (es->verbose && state->unusable[0] != '\0')
		ExplainPropertyText("Offered Not Usable", state->unusable, es);
}

/* A parallel scan: the participants take groups from one shared counter. */
static Size
source_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	return MAXALIGN(sizeof(pg_atomic_uint64));
}

static void
source_initialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	SourceScanState *state = (SourceScanState *) css;

	pg_atomic_init_u64((pg_atomic_uint64 *) coordinate, 0);
	state->gen.shared = coordinate;
}

static void
source_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt, void *coordinate)
{
	SourceScanState *state = (SourceScanState *) css;

	pg_atomic_write_u64((pg_atomic_uint64 *) coordinate, 0);
	state->gen.shared = coordinate;
}

static void
source_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	((SourceScanState *) css)->gen.shared = coordinate;
}

static void
source_shutdown(CustomScanState *css)
{
	((SourceScanState *) css)->gen.shared = NULL;
}

/* ---- the sources ---- */

/* Whether the path is the test wrapper's scan of one of its tables. */
static bool
test_foreign_path(PlannerInfo *root, Path *path)
{
	return IsA(path, ForeignPath) && path->parent->fdwroutine != NULL &&
		path->parent->fdwroutine->GetForeignPaths == fdw_paths;
}

static Oid
path_relid(PlannerInfo *root, Path *path)
{
	return planner_rt_fetch(path->parent->relid, root)->relid;
}

static bool
column_listed(List *names, Oid relid, AttrNumber attno)
{
	char	   *name = get_attname(relid, attno, false);

	foreach_ptr(char, listed, names)
	{
		if (strcmp(listed, name) == 0)
			return true;
	}
	return false;
}

static double
participants(const Path *path)
{
	double		divisor = path->parallel_workers;

	if (parallel_leader_participation && 1.0 - 0.3 * path->parallel_workers > 0)
		divisor += 1.0 - 0.3 * path->parallel_workers;
	return divisor;
}

/*
 * The reader's path for the path offered: the clauses of the columns the
 * table names answered and carried, its rows and costs from the options
 * (the path offered's rows, half its costs, by default), times scale.
 */
static CustomPath *
build_source_path(PlannerInfo *root, Path *path, TessScanClauses *clauses, double scale)
{
	Oid			relid = path_relid(root, path);
	TableOptions options;
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	TessPlanWriter *writer;
	StringInfoData usable;
	StringInfoData unusable;
	List	   *expressions = NIL;
	List	   *answers = NIL;
	bool		partial = path->parallel_workers > 0;
	List	   *exact;
	List	   *prune;
	CustomPath *scan;

	read_options(relid, &options);
	if (partial && options.source_unsafe)
		return NULL;
	exact = partial ? options.partial_exact : options.exact;
	prune = partial ? options.partial_prune : options.prune;
	initStringInfo(&usable);
	initStringInfo(&unusable);
	for (int i = 0; i < clauses->nclauses; i++)
	{
		Expr	   *clause = clauses->clauses[i]->clause;
		AttrNumber	attno;
		int			strategy;
		Expr	   *side;
		StringInfo	list = clauses->usable[i] ? &usable : &unusable;

		appendStringInfo(list, "%s%s", list->len > 0 ? ", " : "",
						 clause_text(clause, path->parent->relid, relid));
		if (!clauses->usable[i] ||
			!clause_shape(clause, path->parent->relid, &attno, &strategy, &side))
			continue;
		if (column_listed(exact, relid, attno))
			clauses->answers[i] = TESS_CLAUSE_EXACT;
		else if (column_listed(prune, relid, attno))
			clauses->answers[i] = TESS_CLAUSE_PRUNE_ONLY;
		else
			continue;
		expressions = lappend(expressions, clause);
		answers = lappend_int(answers, clauses->answers[i]);
	}
	writer = tess_plan_writer_create(DATA_KIND, DATA_VERSION);
	tess_plan_write_string(writer, "path", strVal(linitial(((ForeignPath *) path)->fdw_private)));
	tess_plan_write_int_list(writer, "answers", answers);
	tess_plan_write_int(writer, "own", options.own ? 1 : 0);
	tess_plan_write_string(writer, "usable", usable.data);
	tess_plan_write_string(writer, "unusable", unusable.data);
	config.template_path = path;
	config.methods = &source_path_methods;
	config.node = &scan_node;
	config.expressions = expressions;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	scan = tess_path_create(&config);
	if (options.source_rows >= 0)
		scan->path.rows = clamp_row_est(partial ? options.source_rows / participants(path) :
										options.source_rows);
	if (options.source_total >= 0)
	{
		Cost		startup = Max(options.source_startup, 0);

		scan->path.startup_cost = startup;
		scan->path.total_cost = partial ?
			startup + (options.source_total - startup) / participants(path) :
			options.source_total;
	}
	else
	{
		scan->path.startup_cost = path->startup_cost / 2;
		scan->path.total_cost = path->total_cost / 2;
	}
	scan->path.startup_cost *= scale;
	scan->path.total_cost *= scale;
	if (options.source_unsafe)
		scan->path.parallel_safe = false;
	return scan;
}

static CustomPath *
reader_scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	source_calls[SOURCE_READER]++;
	return test_foreign_path(root, path) ? build_source_path(root, path, clauses, 1.0) : NULL;
}

/* The reader's path at half its cost, for any table of the wrapper. */
static CustomPath *
cheaper_scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	source_calls[SOURCE_CHEAPER]++;
	return test_foreign_path(root, path) ? build_source_path(root, path, clauses, 0.5) : NULL;
}

/* Answers that it checks every clause it may use, then reads nothing. */
static CustomPath *
decliner_scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	source_calls[SOURCE_DECLINER]++;
	for (int i = 0; i < clauses->nclauses; i++)
	{
		if (clauses->usable[i])
			clauses->answers[i] = TESS_CLAUSE_EXACT;
	}
	return NULL;
}

static CustomPath *
none_scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	source_calls[SOURCE_NONE]++;
	return NULL;
}

static CustomPath *
past_scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	source_calls[SOURCE_PAST]++;
	return NULL;
}

/* The first column's ascending order, as path keys. */
static List *
first_column_pathkeys(PlannerInfo *root, RelOptInfo *rel)
{
	Oid			reloid = planner_rt_fetch(rel->relid, root)->relid;
	Var		   *var = makeVar(rel->relid, 1, INT4OID, -1, InvalidOid, 0);

	if (get_atttype(reloid, 1) != INT4OID || GetForeignColumnOptions(reloid, 1) != NIL)
		elog(ERROR, "the test source orders only by an int4 first column of i itself");
	return build_expression_pathkey(root, (Expr *) var, Int4LessOperator, rel->relids, true);
}

/*
 * The reader's path broken as the fault says, for the offer it concerns:
 * the serial or the partial one; the reader's path for the other offer.
 */
static CustomPath *
faulty_scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	bool		partial = path->parallel_workers > 0;
	CustomPath *scan;

	source_calls[SOURCE_FAULTY]++;
	if (!test_foreign_path(root, path) || fault == NULL)
		return NULL;
	if (strcmp(fault, "core") == 0)
		return (CustomPath *) path;
	if (strcmp(fault, "other_custom") == 0)
	{
		scan = makeNode(CustomPath);
		scan->path = *path;
		NodeSetTag(scan, T_CustomPath);
		scan->path.pathtype = T_CustomScan;
		scan->flags = CUSTOMPATH_SUPPORT_PROJECTION;
		scan->methods = &other_path_methods;
		return scan;
	}
	scan = build_source_path(root, path, clauses, 1.0);
	if (scan == NULL)
		return NULL;
	if (strcmp(fault, "other_relation") == 0)
	{
		RelOptInfo *other = makeNode(RelOptInfo);

		other->relids = bms_make_singleton(path->parent->relid + 1);
		scan->path.parent = other;
	}
	else if (strcmp(fault, "params") == 0)
	{
		ParamPathInfo *param = makeNode(ParamPathInfo);

		param->ppi_req_outer = bms_make_singleton(path->parent->relid + 1);
		scan->path.param_info = param;
	}
	else if (strcmp(fault, "pathkeys") == 0)
		scan->path.pathkeys = first_column_pathkeys(root, path->parent);
	else if (strcmp(fault, "no_projection") == 0)
		scan->flags &= ~CUSTOMPATH_SUPPORT_PROJECTION;
	else if (strcmp(fault, "serial_for_partial") == 0 && partial)
	{
		scan->path.parallel_aware = false;
		scan->path.parallel_workers = 0;
	}
	else if (strcmp(fault, "partial_for_serial") == 0 && !partial)
	{
		scan->path.parallel_aware = true;
		scan->path.parallel_workers = 2;
	}
	else if (strcmp(fault, "partial_unaware") == 0 && partial)
		scan->path.parallel_aware = false;
	else if (strcmp(fault, "partial_unsafe") == 0 && partial)
		scan->path.parallel_safe = false;
	else if (strcmp(fault, "workers") == 0 && partial)
		scan->path.parallel_workers++;
	else if (strcmp(fault, "serial_safe") == 0 && !partial)
		scan->path.parallel_safe = true;
	else if (strncmp(fault, "unusable", 8) == 0 || strncmp(fault, "not_carried", 11) == 0 ||
			 strcmp(fault, "bad_value") == 0)
	{
		TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
		List	   *expressions = NIL;
		CustomPath *broken;
		TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);

		for (int i = 0; i < clauses->nclauses; i++)
			clauses->answers[i] = TESS_CLAUSE_UNSUPPORTED;
		for (int i = 0; i < clauses->nclauses; i++)
		{
			bool		unusable = strncmp(fault, "unusable", 8) == 0;

			if (clauses->usable[i] == unusable)
				continue;
			clauses->answers[i] = strcmp(fault, "bad_value") == 0 ? 7 :
				strstr(fault, "_prune") != NULL ? TESS_CLAUSE_PRUNE_ONLY : TESS_CLAUSE_EXACT;
			if (strncmp(fault, "not_carried", 11) != 0)
				expressions = list_make1(clauses->clauses[i]->clause);
			break;
		}
		tess_path_get_info(scan, &info);
		config.template_path = &scan->path;
		config.methods = &source_path_methods;
		config.node = &scan_node;
		config.expressions = expressions;
		config.node_data = info.node_data;
		config.flags = scan->flags;
		broken = tess_path_create(&config);
		return broken;
	}
	return scan;
}

/*
 * A path for whatever path it is offered, heap or foreign, using no
 * clause: no test may see it in a plan, since Tessera offers a source a
 * foreign table's path alone.
 */
static CustomPath *
any_scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	TessPlanWriter *writer = tess_plan_writer_create(DATA_KIND, DATA_VERSION);
	CustomPath *scan;

	source_calls[SOURCE_ANY]++;
	tess_plan_write_string(writer, "path", "any");
	tess_plan_write_int_list(writer, "answers", NIL);
	tess_plan_write_int(writer, "own", 0);
	tess_plan_write_string(writer, "usable", "");
	tess_plan_write_string(writer, "unusable", "");
	config.template_path = path;
	config.methods = &source_path_methods;
	config.node = &scan_node;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	config.flags = CUSTOMPATH_SUPPORT_PROJECTION;
	scan = tess_path_create(&config);
	scan->path.startup_cost = 0;
	scan->path.total_cost = path->total_cost / 100;
	return scan;
}

/* Raises: a source of the least size has no scan_rows to call. */
static CustomPath *
least_scan_rows(PlannerInfo *root, Path *path, TessScanClauses *clauses)
{
	elog(ERROR, "Tessera read past the size of a source");
	return NULL;
}

static const TessSource reader_source = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.reader",
	.scan_rows = reader_scan_rows,
};

static const TessSource cheaper_source = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.cheaper",
	.scan_rows = cheaper_scan_rows,
};

static const TessSource decliner_source = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.decliner",
	.scan_rows = decliner_scan_rows,
};

static const TessSource none_source = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.none",
	.scan_rows = none_scan_rows,
};

/* Its size ends at name: the callback after it must never be read. */
static const TessSource least_source = {
	.abi_version = TESS_SOURCE_ABI_VERSION,
	.struct_size = TESS_SOURCE_MIN_SIZE,
	.name = "tessera_test.least",
	.scan_rows = least_scan_rows,
};

static const TessSource null_callback_source = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.null_callback",
};

/* A source of a later version of the structure, one field past it. */
typedef struct PastSource
{
	TessSource	base;
	void	   *later_field;
} PastSource;

static const PastSource past_source = {
	.base = {
		.abi_version = TESS_SOURCE_ABI_VERSION,
		.struct_size = sizeof(PastSource),
		.name = "tessera_test.past",
		.scan_rows = past_scan_rows,
	},
};

static const TessSource faulty_source = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.faulty",
	.scan_rows = faulty_scan_rows,
};

static const TessSource any_source = {
	TESS_ABI_INITIALIZER(TESS_SOURCE_ABI_VERSION, TessSource),
	.name = "tessera_test.any",
	.scan_rows = any_scan_rows,
};

static const TessSource *const sources[NSOURCES] = {
	&reader_source, &cheaper_source, &decliner_source, &none_source, &least_source,
	&null_callback_source, &past_source.base, &faulty_source, &any_source,
};

/*
 * Register the sources named, in their order, after removing every
 * source of this module, and set their calls to zero.
 */
Datum
tessera_test_scan_sources(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *registry =
		tess_source_registry_check(tess_runtime_api()->sources);
	ArrayType  *names = PG_GETARG_ARRAYTYPE_P(0);
	Datum	   *elements;
	bool	   *nulls;
	int			count;

	deconstruct_array_builtin(names, TEXTOID, &elements, &nulls, &count);
	for (int i = 0; i < count; i++)
	{
		if (nulls[i])
			elog(ERROR, "a test source has no NULL name");
	}
	for (int s = 0; s < NSOURCES; s++)
	{
		registry->remove(sources[s]);
		source_calls[s] = 0;
	}
	for (int i = 0; i < count; i++)
	{
		char	   *name = TextDatumGetCString(elements[i]);
		int			s = 0;

		while (s < NSOURCES && strcmp(source_names[s], name) != 0)
			s++;
		if (s == NSOURCES)
			elog(ERROR, "no test source \"%s\"", name);
		registry->add(sources[s]);
	}
	PG_RETURN_VOID();
}

/* The calls of each source since they were registered. */
Datum
tessera_test_scan_source_calls(PG_FUNCTION_ARGS)
{
	StringInfoData calls;

	initStringInfo(&calls);
	for (int s = 0; s < NSOURCES; s++)
	{
		if (source_calls[s] > 0)
			appendStringInfo(&calls, "%s%s %d", calls.len > 0 ? ", " : "",
							 source_names[s], source_calls[s]);
	}
	PG_RETURN_TEXT_P(cstring_to_text(calls.data));
}

/*
 * The asks of each form of the source's batches, and the batches its own
 * checks read, of the form and through a projection, since the last call,
 * in the calling process.
 */
Datum
tessera_test_scan_source_asks(PG_FUNCTION_ARGS)
{
	char	   *asks = psprintf("datum " INT64_FORMAT ", bytes " INT64_FORMAT
								", checked " INT64_FORMAT ", through a projection " INT64_FORMAT,
								datum_asks, bytes_asks, bytes_checks, projection_checks);

	datum_asks = 0;
	bytes_asks = 0;
	bytes_checks = 0;
	projection_checks = 0;
	PG_RETURN_TEXT_P(cstring_to_text(asks));
}

/*
 * The sources the registry lists, in the order of next, each found by its
 * name, as the planner sees them.
 */
Datum
tessera_test_scan_source_listed(PG_FUNCTION_ARGS)
{
	const TessSourceRegistryOps *registry =
		tess_source_registry_check(tess_runtime_api()->sources);
	StringInfoData names;

	initStringInfo(&names);
	for (const TessSource *source = registry->next(NULL); source != NULL;
		 source = registry->next(source))
		appendStringInfo(&names, "%s%s%s", names.len > 0 ? ", " : "", source->name,
						 registry->find(source->name) == source ? "" : " (not found)");
	PG_RETURN_TEXT_P(cstring_to_text(names.data));
}

/* The fault the faulty source and the node's plan make; NULL for none. */
Datum
tessera_test_scan_source_fault(PG_FUNCTION_ARGS)
{
	if (fault != NULL)
		pfree(fault);
	fault = PG_ARGISNULL(0) ? NULL :
		MemoryContextStrdup(TopMemoryContext, text_to_cstring(PG_GETARG_TEXT_PP(0)));
	PG_RETURN_VOID();
}

/* ---- the foreign data wrapper ---- */

typedef struct FdwScanState
{
	Generator	gen;
	MemoryContext row_context;
	/* A partial path that is not parallel-aware: the leader reads alone. */
	bool		leader_only;
} FdwScanState;

static void
fdw_rel_size(PlannerInfo *root, RelOptInfo *baserel, Oid foreigntableid)
{
	TableOptions options;

	read_options(foreigntableid, &options);
	baserel->tuples = options.rows;
	baserel->rows = clamp_row_est(options.rows *
								  clauselist_selectivity(root, baserel->baserestrictinfo,
														 0, JOIN_INNER, NULL));
}

/* A path that needs required_outer and the relation's lateral references. */
static ForeignPath *
make_path(PlannerInfo *root, RelOptInfo *baserel, double rows, Cost startup, Cost total,
		  List *pathkeys, Relids required_outer, const char *tag)
{
	return create_foreignscan_path(root, baserel, NULL, rows, 0, startup, total, pathkeys,
								   bms_union(required_outer, baserel->lateral_relids),
								   NULL, NIL, list_make1(makeString(pstrdup(tag))));
}

/*
 * The paths of the options: a first one, a second, both with path keys or
 * without; one parameterized by the join clauses of each other relation;
 * a partial one, where the relation has no lateral references. A partial
 * path that is not parallel-aware is there for the planner to see and
 * never to run: it is disabled, and the leader alone would read it.
 */
static void
fdw_paths(PlannerInfo *root, RelOptInfo *baserel, Oid foreigntableid)
{
	TableOptions options;
	List	   *pathkeys = NIL;
	List	   *outers = NIL;

	read_options(foreigntableid, &options);
	if (options.pathkeys)
		pathkeys = first_column_pathkeys(root, baserel);
	add_path(baserel, (Path *) make_path(root, baserel, baserel->rows, options.startup,
										 options.total, pathkeys, NULL, "first"));
	if (options.second)
		add_path(baserel, (Path *) make_path(root, baserel, baserel->rows, options.startup2,
											 options.total2, pathkeys, NULL, "second"));
	if (options.parameterized)
	{
		foreach_node(RestrictInfo, rinfo, baserel->joininfo)
		{
			Relids		outer = bms_difference(rinfo->clause_relids, baserel->relids);
			ParamPathInfo *param;

			if (bms_is_empty(outer) || list_member(outers, outer))
				continue;
			outers = lappend(outers, outer);
			param = get_baserel_parampathinfo(root, baserel,
											  bms_union(outer, baserel->lateral_relids));
			add_path(baserel, (Path *) make_path(root, baserel, param->ppi_rows, 0,
												 param->ppi_rows * 0.01, NIL, outer,
												 "parameterized"));
		}
	}
	if (options.partial != 0 && baserel->consider_parallel && options.workers > 0 &&
		bms_is_empty(baserel->lateral_relids))
	{
		ForeignPath *path = make_path(root, baserel, baserel->rows, options.startup,
									  options.total, NIL, NULL,
									  options.partial == 1 ? "partial" : "partial unaware");
		double		divisor;

		path->path.parallel_aware = options.partial == 1;
		path->path.disabled_nodes = options.partial == 1 ? 0 : 1;
		path->path.parallel_workers = options.workers;
		divisor = participants(&path->path);
		path->path.rows = clamp_row_est(baserel->rows / divisor);
		path->path.total_cost = options.startup + (options.total - options.startup) / divisor;
		add_partial_path(baserel, (Path *) path);
	}
}

static ForeignScan *
fdw_plan(PlannerInfo *root, RelOptInfo *baserel, Oid foreigntableid, ForeignPath *best_path,
		 List *tlist, List *scan_clauses, Plan *outer_plan)
{
	return make_foreignscan(tlist, extract_actual_clauses(scan_clauses, false),
							baserel->relid, NIL, best_path->fdw_private, NIL, NIL, outer_plan);
}

static void
fdw_explain(ForeignScanState *node, ExplainState *es)
{
	ForeignScan *plan = castNode(ForeignScan, node->ss.ps.plan);

	ExplainPropertyText("Foreign Path", strVal(linitial(plan->fdw_private)), es);
}

static void
fdw_begin(ForeignScanState *node, int eflags)
{
	ForeignScan *plan = castNode(ForeignScan, node->ss.ps.plan);
	FdwScanState *state;

	if ((eflags & EXEC_FLAG_EXPLAIN_ONLY) != 0)
		return;
	state = palloc0_object(FdwScanState);
	generator_init(&state->gen, node->ss.ss_currentRelation);
	state->row_context = AllocSetContextCreate(node->ss.ps.state->es_query_cxt,
											   "TessTestFdw row", ALLOCSET_SMALL_SIZES);
	state->leader_only = strcmp(strVal(linitial(plan->fdw_private)), "partial unaware") == 0;
	node->fdw_state = state;
}

static TupleTableSlot *
fdw_iterate(ForeignScanState *node)
{
	FdwScanState *state = node->fdw_state;
	TupleTableSlot *slot = node->ss.ss_ScanTupleSlot;
	Generator  *gen = &state->gen;

	MemoryContext old;

	ExecClearTuple(slot);
	MemoryContextReset(state->row_context);
	if (state->leader_only && IsParallelWorker())
		return slot;
	if (gen->row > gen->last && !generator_next_group(gen))
		return slot;
	old = MemoryContextSwitchTo(state->row_context);
	generator_row(gen, gen->row, slot->tts_values, slot->tts_isnull);
	MemoryContextSwitchTo(old);
	gen->row++;
	return ExecStoreVirtualTuple(slot);
}

static void
fdw_rescan(ForeignScanState *node)
{
	generator_rescan(&((FdwScanState *) node->fdw_state)->gen);
}

static void
fdw_end(ForeignScanState *node)
{
}

static bool
fdw_parallel_safe(PlannerInfo *root, RelOptInfo *rel, RangeTblEntry *rte)
{
	TableOptions options;

	read_options(rte->relid, &options);
	return options.parallel_safe;
}

static Size
fdw_estimate_dsm(ForeignScanState *node, ParallelContext *pcxt)
{
	return MAXALIGN(sizeof(pg_atomic_uint64));
}

static void
fdw_initialize_dsm(ForeignScanState *node, ParallelContext *pcxt, void *coordinate)
{
	pg_atomic_init_u64((pg_atomic_uint64 *) coordinate, 0);
	((FdwScanState *) node->fdw_state)->gen.shared = coordinate;
}

static void
fdw_reinitialize_dsm(ForeignScanState *node, ParallelContext *pcxt, void *coordinate)
{
	pg_atomic_write_u64((pg_atomic_uint64 *) coordinate, 0);
	((FdwScanState *) node->fdw_state)->gen.shared = coordinate;
}

static void
fdw_initialize_worker(ForeignScanState *node, shm_toc *toc, void *coordinate)
{
	((FdwScanState *) node->fdw_state)->gen.shared = coordinate;
}

static void
fdw_shutdown(ForeignScanState *node)
{
	if (node->fdw_state != NULL)
		((FdwScanState *) node->fdw_state)->gen.shared = NULL;
}

Datum
tessera_test_scan_fdw_handler(PG_FUNCTION_ARGS)
{
	FdwRoutine *routine = makeNode(FdwRoutine);

	routine->GetForeignRelSize = fdw_rel_size;
	routine->GetForeignPaths = fdw_paths;
	routine->GetForeignPlan = fdw_plan;
	routine->ExplainForeignScan = fdw_explain;
	routine->BeginForeignScan = fdw_begin;
	routine->IterateForeignScan = fdw_iterate;
	routine->ReScanForeignScan = fdw_rescan;
	routine->EndForeignScan = fdw_end;
	routine->IsForeignScanParallelSafe = fdw_parallel_safe;
	routine->EstimateDSMForeignScan = fdw_estimate_dsm;
	routine->InitializeDSMForeignScan = fdw_initialize_dsm;
	routine->ReInitializeDSMForeignScan = fdw_reinitialize_dsm;
	routine->InitializeWorkerForeignScan = fdw_initialize_worker;
	routine->ShutdownForeignScan = fdw_shutdown;
	PG_RETURN_POINTER(routine);
}

void
_PG_init(void)
{
	tess_runtime_api()->nodes->add(&scan_node);
	RegisterCustomScanMethods(&source_scan_methods);
}

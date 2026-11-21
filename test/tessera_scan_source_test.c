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

#define GROUP_ROWS 64
#define NODE_NAME "tessera_test.scan_source"
#define DATA_KIND "tessera_test.scan_source"
#define DATA_VERSION 1

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
} TableOptions;

/* How one column's values are made. */
typedef struct ColumnGen
{
	Oid			type;
	int64		mod;
	int64		nulls;
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
			else
				elog(ERROR, "unknown column option \"%s\"", def->defname);
		}
	}
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
		{
			char		text[32];

			snprintf(text, sizeof(text), INT64_FORMAT, value);
			values[c] = PointerGetDatum(cstring_to_text(text));
		}
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
	if (batch != NULL && state->projection != NULL)
		batch = tess_projection_wrap(state->projection, batch);
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

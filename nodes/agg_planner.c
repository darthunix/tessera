#include "postgres.h"

#include <math.h>

#include "access/htup_details.h"
#include "catalog/pg_aggregate.h"
#include "catalog/pg_class.h"
#include "catalog/pg_language.h"
#include "catalog/pg_proc.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/cost.h"
#include "optimizer/optimizer.h"
#include "optimizer/pathnode.h"
#include "optimizer/plancat.h"
#include "optimizer/clauses.h"
#include "optimizer/planner.h"
#include "optimizer/prep.h"
#include "optimizer/tlist.h"
#include "parser/parse_oper.h"
#include "utils/fmgroids.h"
#include "utils/syscache.h"
#include "utils/typcache.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "utils/fmgrprotos.h"
#include "utils/numeric.h"
#include "utils/selfuncs.h"

#include "tessera/expr.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"
#include "agg.h"

/*
 * The planner of TessAgg: which aggregates and keys the node takes, the
 * paths it offers for the core's grouping, DISTINCT and set operations,
 * serial and partial, their cost, and the plan it writes for the executor
 * (agg.c). See docs/nodes.md.
 */

/*
 * A path's flags in its private data: the node is the query's grouping,
 * whose plan applies HAVING (a DISTINCT or a set operation above one must
 * not); the node groups for a Gather, its table emptied early; the node
 * groups above one, merging the participants' partial values; the partial
 * values of its sum states are the node's own format, which only its final
 * grouping reads (TessTableSumInput).
 */
#define AGG_PATH_HAVING 0x01
#define AGG_PATH_PARTIAL 0x02
#define AGG_PATH_FINALIZE 0x04
#define AGG_PATH_OWN_STATES 0x08

/*
 * A grouping whose DISTINCT pairs do not fit hash_mem by the planner's
 * estimate, and go to disk: the bytes of its pairs, of each set and of
 * them all, and of its groups, as the planner estimates them.
 */
typedef struct PairsEstimate
{
	double		pairs;
	double		groups;
	int			sets;
	/* Of each DISTINCT aggregate, as many as the states have. */
	double	   *set_pairs;
} PairsEstimate;

static create_upper_paths_hook_type previous_create_upper_paths_hook = NULL;

static Plan *agg_plan(PlannerInfo *root, RelOptInfo *rel,
					  CustomPath *best_path, List *tlist, List *clauses,
					  List *custom_plans);

static const CustomPathMethods agg_path_methods = {
	.CustomName = "TessAgg",
	.PlanCustomPath = agg_plan,
};

static Node *aggregate_argument(const Aggref *agg);
static bool key_eqop(Node *key, List *clauses, int *eqop);
static void create_nonunion_paths(PlannerInfo *root, RelOptInfo *output_rel);

/*
 * The kind of a supported aggregate, or -1: the node knows how to combine
 * these. The extremes are those of the types a word holds, compared as
 * the word's integer: bool_and and every the least boolean, bool_or the
 * greatest.
 */
int
aggregate_kind(Oid aggfnoid)
{
	switch (aggfnoid)
	{
		case F_COUNT_:
		case F_COUNT_ANY:
			return AGG_COUNT;
		case F_SUM_INT4:
			return AGG_SUM;
		case F_MIN_INT2:
		case F_MIN_INT4:
		case F_MIN_INT8:
		case F_MIN_DATE:
		case F_MIN_TIMESTAMP:
		case F_MIN_TIMESTAMPTZ:
		case F_MIN_TIME:
		case F_MIN_MONEY:
		case F_BOOL_AND:
		case F_EVERY:
			return AGG_MIN;
		case F_MAX_INT2:
		case F_MAX_INT4:
		case F_MAX_INT8:
		case F_MAX_DATE:
		case F_MAX_TIMESTAMP:
		case F_MAX_TIMESTAMPTZ:
		case F_MAX_TIME:
		case F_MAX_MONEY:
		case F_BOOL_OR:
			return AGG_MAX;
		default:
			return -1;
	}
}

/* Whether a function is a C function of a loadable library. */
static bool
library_function(Oid function)
{
	HeapTuple	tuple;
	bool		library;

	if (!OidIsValid(function))
		return false;
	tuple = SearchSysCache1(PROCOID, ObjectIdGetDatum(function));
	if (!HeapTupleIsValid(tuple))
		elog(ERROR, "cache lookup failed for function %u", function);
	library = ((Form_pg_proc) GETSTRUCT(tuple))->prolang == ClanguageId;
	ReleaseSysCache(tuple);
	return library;
}

/*
 * An aggregate the node computes through the core's functions: a whole
 * one or the partial one of a parallel plan, of arguments without a
 * subplan (DISTINCT: distinct_supported). Not one whose functions include
 * a C function of a loadable library, as an extension's are (the core's
 * own are internal): it may ask its call context for more than the
 * node's stand-in AggState holds (AggGetAggref has no Aggref there), so
 * the core's aggregate computes it.
 */
bool
generic_supported(const Aggref *agg)
{
	HeapTuple	tuple;
	Form_pg_aggregate form;
	bool		library;

	if (agg->args == NIL)
		return false;
	tuple = SearchSysCache1(AGGFNOID, ObjectIdGetDatum(agg->aggfnoid));
	if (!HeapTupleIsValid(tuple))
		elog(ERROR, "cache lookup failed for aggregate %u", agg->aggfnoid);
	form = (Form_pg_aggregate) GETSTRUCT(tuple);
	library = library_function(form->aggtransfn) || library_function(form->aggfinalfn) ||
		library_function(form->aggcombinefn) || library_function(form->aggserialfn) ||
		library_function(form->aggdeserialfn);
	ReleaseSysCache(tuple);
	if (library)
		return false;
	foreach_node(TargetEntry, entry, agg->args)
	{
		if (contain_subplans((Node *) entry->expr))
			return false;
	}
	return true;
}

/*
 * DISTINCT in an aggregate of one argument: a table of the pairs of group
 * and argument seen. count over any type whose equality hashes, a value a
 * word does not hold by its number in a dictionary; sum and avg over
 * integers, whose value neither the order of the values nor which of equal
 * ones comes first changes, unlike string_agg's order, a float's sum or
 * the scale of a numeric one. The node's own extremes keep no pairs, their
 * DISTINCT changing nothing (distinct_matters).
 */
static bool
distinct_supported(const Aggref *agg)
{
	SortGroupClause *clause;
	TessTableKeyKind kind;
	RegProcedure hashproc;

	if (list_length(agg->args) != 1 || list_length(agg->aggdistinct) != 1)
		return false;
	clause = linitial_node(SortGroupClause, agg->aggdistinct);
	switch (agg->aggfnoid)
	{
		case F_COUNT_ANY:
			return tess_word_key_kind(exprType(aggregate_argument(agg)), &kind) ||
				(OidIsValid(clause->eqop) && get_op_hash_functions(clause->eqop, &hashproc, NULL));
		case F_SUM_INT2:
		case F_SUM_INT4:
		case F_SUM_INT8:
		case F_AVG_INT2:
		case F_AVG_INT4:
		case F_AVG_INT8:
			return true;
		default:
			return false;
	}
}

/*
 * Whether an aggregate's DISTINCT changes its answer: not in an extreme the
 * node computes as its own, whose values are words, two equal ones the same
 * bits, so the least of the distinct values is the least of all, as
 * PostgreSQL's planner reads such an aggregate from an index (planagg.c).
 * The node computes it as without DISTINCT, and keeps no pairs for it.
 */
bool
distinct_matters(const Aggref *agg)
{
	int			kind;

	if (agg->aggdistinct == NIL)
		return false;
	kind = aggregate_kind(agg->aggfnoid);
	return !((kind == AGG_MIN || kind == AGG_MAX) && batch_aggregate(agg));
}

/* Whether a batch function computes the aggregate: else the core's do. */
bool
batch_aggregate(const Aggref *agg)
{
	const TessFunction *function = tess_runtime_api()->functions->find(agg->aggfnoid);
	TessTableKeyKind kind;

	if (aggregate_kind(agg->aggfnoid) < 0 || function == NULL ||
		function->kind != TESS_FUNCTION_AGGREGATE)
		return false;
	if (agg->aggfnoid == F_COUNT_ || agg->aggfnoid == F_COUNT_ANY)
		return true;
	return tess_word_key_kind(exprType(aggregate_argument(agg)), &kind);
}

/*
 * Whether a grouping keeps the aggregate as a sum state, words of the
 * group's record that the kernels fold (agg_generic_init): sum and avg of
 * numeric, with the kernels module, and of bigint, avg of integer and
 * smallint, sum of smallint. The planner costs such an aggregate as the
 * node's own.
 */
bool
sum_state_aggregate(const Aggref *agg)
{
#ifdef HAVE_INT128
	if (list_length(agg->args) != 1)
		return false;
	switch (agg->aggfnoid)
	{
		case F_SUM_NUMERIC:
		case F_AVG_NUMERIC:
			return tess_runtime_kernels() != NULL;
		case F_SUM_INT8:
		case F_AVG_INT8:
		case F_SUM_INT2:
		case F_AVG_INT4:
		case F_AVG_INT2:
			return true;
		default:
			break;
	}
#endif
	return false;
}

/*
 * Whether a plain partial aggregate the node folds itself goes up in the
 * node's own format (fast_partial): sum and avg of numeric and bigint, whose
 * state in the core is internal, which only the core's functions write.
 * The node writes the others' states as the core's own transition values.
 */
bool
own_partial_aggregate(const Aggref *agg)
{
	return agg->aggtranstype == INTERNALOID && sum_state_aggregate(agg);
}

/* The aggregates of the target list whose partial values are the node's own format. */
static int
own_partials(List *tlist)
{
	int			count = 0;

	foreach_node(TargetEntry, entry, tlist)
		if (IsA(entry->expr, Aggref) && own_partial_aggregate((Aggref *) entry->expr))
			count++;
	return count;
}

/*
 * The bytes the groups of a grouping with a DISTINCT aggregate take, as
 * the planner estimates them, or -1 without a grouped path to count them
 * by; such groups do not spill. A group takes a record, a sum state's
 * four more words, a float's state of the node's own, and the states a
 * word does not hold, each by its type's average width or, for an
 * internal state, the aggregate's declared space or 8 kB, as the core
 * estimates its hashed groups. The index takes the two buckets of 4 bytes
 * a group the node gives it, a power of two of them and 1024 at least,
 * counted in floating point, since an estimate may pass any power a word
 * holds. The groups are the most of the core's grouped paths, before
 * HAVING: the relation's own rows are set only after this hook, and
 * beside a DISTINCT aggregate the core's paths are sorted ones alone.
 */
static double
groups_bytes(PlannerInfo *root, RelOptInfo *output_rel, int nkeys, List *tlist)
{
	double		groups = 0;
	double		bytes;

	foreach_ptr(Path, path, output_rel->pathlist)
		if (IsA(path, AggPath))
			groups = Max(groups, ((AggPath *) path)->numGroups);
	if (groups <= 0)
		return -1;
	/* A record of the keys and its words of flags. */
	bytes = (double) tess_table_record_bytes(nkeys, sizeof(uint64) *
											  AGG_FLAG_WORDS(list_length(tlist) - nkeys));
	foreach_node(TargetEntry, entry, tlist)
	{
		Aggref	   *agg = (Aggref *) entry->expr;
		int16		len;
		bool		byval;

		if (!IsA(agg, Aggref))
			continue;
		bytes += 8.0;
		if (batch_aggregate(agg))
			continue;
		if (sum_state_aggregate(agg))
		{
			bytes += 8.0 * (AGG_SUM_STATE_WORDS - 1);
			continue;
		}
		if (agg_float_state_bytes(agg) > 0)
		{
			bytes += (double) agg_float_state_bytes(agg);
			continue;
		}
		/* Before byval: internal passes by value, its state lies elsewhere. */
		if (agg->aggtranstype == INTERNALOID)
		{
			HeapTuple	tuple = SearchSysCache1(AGGFNOID, ObjectIdGetDatum(agg->aggfnoid));
			int32		space = 0;

			if (HeapTupleIsValid(tuple))
			{
				space = ((Form_pg_aggregate) GETSTRUCT(tuple))->aggtransspace;
				ReleaseSysCache(tuple);
			}
			bytes += space > 0 ? space : ALLOCSET_DEFAULT_INITSIZE;
			continue;
		}
		get_typlenbyval(agg->aggtranstype, &len, &byval);
		if (!byval)
			bytes += get_typavgwidth(agg->aggtranstype, -1);
	}
	return groups * bytes + 4.0 * pow(2.0, ceil(log2(Max(2.0 * groups, 1024.0))));
}

/*
 * The sum states a grouping of the target list keeps, or -1 when another
 * aggregate of it calls the core's transition function row by row.
 */
static int
sum_states(List *tlist)
{
	int			count = 0;

	foreach_node(TargetEntry, entry, tlist)
	{
		Aggref	   *agg = (Aggref *) entry->expr;

		if (!IsA(agg, Aggref) || batch_aggregate(agg))
			continue;
		if (!sum_state_aggregate(agg))
			return -1;
		count++;
	}
	return count;
}

/* The aggregated argument, or NULL for count(*). */
static Node *
aggregate_argument(const Aggref *agg)
{
	return agg->args == NIL ? NULL :
		(Node *) ((TargetEntry *) linitial(agg->args))->expr;
}

/*
 * Whether the node computes this aggregate: a plain call, whole or the
 * partial one of a parallel plan, of an aggregate the node combines and
 * the registry implements over batches, with no argument for count(*),
 * one expression of any type for count (the count reads NULL flags
 * alone) or one expression of a type a word holds for the others, which the
 * projection provider computes by a chain or row by row, and a FILTER
 * condition, which it computes too; a subplan in them would need fixing
 * against the scan tuple, which the arguments do not go through.
 */
static bool
aggregate_supported(const Aggref *agg)
{
	Node	   *argument;

	if (agg->agglevelsup != 0 || agg->aggkind != AGGKIND_NORMAL ||
		(agg->aggsplit != AGGSPLIT_SIMPLE &&
		 agg->aggsplit != AGGSPLIT_INITIAL_SERIAL) || agg->aggorder != NIL ||
		contain_subplans((Node *) agg->aggfilter) ||
		agg->aggdirectargs != NIL || agg->aggvariadic ||
		(distinct_matters(agg) && !distinct_supported(agg)))
		return false;
	if (!batch_aggregate(agg))
		return generic_supported(agg);
	if (agg->aggfnoid == F_COUNT_)
		return agg->aggstar && agg->args == NIL;
	argument = aggregate_argument(agg);
	if (list_length(agg->args) != 1 || contain_subplans(argument))
		return false;
	return true;
}

/*
 * Whether an expression reads something the child's target lacks: a
 * subexpression the target holds whole, as the planner puts a grouping
 * expression there, or a column of it are available; a placeholder
 * would stay one in the private data, so none is accepted.
 */
static bool
unavailable(Node *node, List *exprs)
{
	if (node == NULL)
		return false;
	if (list_member(exprs, node))
		return false;
	if (IsA(node, Var) || IsA(node, PlaceHolderVar))
		return true;
	return expression_tree_walker(node, unavailable, exprs);
}

/*
 * Above a gather: whether the child's target gives the keys and each
 * aggregate's partial value, which the node merges.
 */
static bool
partials_available(const List *tlist, const Path *child)
{
	foreach_ptr(TargetEntry, entry, tlist)
	{
		Node	   *expr = (Node *) entry->expr;

		if (IsA(expr, Aggref))
		{
			Aggref	   *partial = copyObject((Aggref *) expr);

			mark_partial_aggref(partial, AGGSPLIT_INITIAL_SERIAL);
			expr = (Node *) partial;
		}
		if (!list_member(child->pathtarget->exprs, expr))
			return false;
	}
	return true;
}

/* Whether the child's target gives what the keys and the arguments read. */
static bool
arguments_available(const List *tlist, const Path *child)
{
	foreach_ptr(TargetEntry, entry, tlist)
	{
		Node	   *argument = IsA(entry->expr, Aggref) ?
			(Node *) list_make2(((Aggref *) entry->expr)->args,
								((Aggref *) entry->expr)->aggfilter) :
			(Node *) entry->expr;

		if (unavailable(argument, child->pathtarget->exprs))
			return false;
	}
	return true;
}

/*
 * Whether an expression above the aggregation is made of what the scan
 * tuple holds: grouping expressions, aggregates and constants, as the
 * planner will rewrite it; a column outside them, such as one the
 * primary key makes functionally dependent, is not.
 */
static bool
not_from_groups(Node *node, List *keys)
{
	if (node == NULL || IsA(node, Aggref))
		return false;
	foreach_ptr(Node, key, keys)
	{
		if (equal(node, key))
			return false;
	}
	if (IsA(node, Var) || IsA(node, PlaceHolderVar))
		return true;
	return expression_tree_walker(node, not_from_groups, keys);
}

/*
 * Append the distinct aggregates of the expressions to a flat target
 * list, the scan tuple of the node after the grouping expressions; false
 * when one is not supported, or an expression reads a column neither a
 * grouping expression nor an aggregate gives. Expressions above the
 * aggregates are left to the plan's projection and qual over that tuple.
 */
static bool
collect_aggregates(Node *expressions, List *keys, List **tlist)
{
	List	   *found;

	if (keys != NIL && not_from_groups(expressions, keys))
		return false;
	found = pull_var_clause(expressions, PVC_INCLUDE_AGGREGATES |
							PVC_RECURSE_PLACEHOLDERS);
	foreach_ptr(Node, node, found)
	{
		if (IsA(node, Aggref))
		{
			if (!aggregate_supported((Aggref *) node))
				return false;
			*tlist = add_to_flat_tlist(*tlist, list_make1(node));
		}
		else if (keys == NIL)
			return false;
	}
	return true;
}

/*
 * The queries the node handles: plain aggregation of a single result row,
 * or grouping by plain grouping expressions.
 */
static bool
query_supported(PlannerInfo *root, RelOptInfo *input_rel, RelOptInfo *output_rel)
{
	Query	   *parse = root->parse;

	return (parse->hasAggs || parse->groupClause != NIL) &&
		parse->groupingSets == NIL && !parse->hasWindowFuncs &&
		output_rel->reloptkind == RELOPT_UPPER_REL && !IS_DUMMY_REL(input_rel);
}

/*
 * The expressions of grouping or distinct clauses when the node can group
 * by them: 1 to 16 values of a type the table keeps in a word
 * (tess_word_key_kind) or of any type its equality hashes, a bare column, a chain the expression compiler
 * takes such as c % 10, or any other expression, computed row by row. NIL
 * otherwise, also when the planner dropped every grouping clause, as for
 * a constant one.
 */
static List *
clause_keys(PlannerInfo *root, List *clauses)
{
	List	   *keys = NIL;

	if (clauses == NIL || list_length(clauses) > TESS_TABLE_MAX_KEYS)
		return NIL;
	foreach_node(SortGroupClause, clause, clauses)
	{
		Node	   *expr = (Node *) get_sortgroupclause_expr(clause,
															root->processed_tlist);
		TessTableKeyKind kind;

		/*
		 * A key the compiler does not take is computed row by row; one of a
		 * type a word does not hold goes through a dictionary of its
		 * values by its hash and equality functions (KeyDict).
		 */
		if ((!tess_word_key_kind(exprType(expr), &kind) && !clause->hashable) ||
			contain_subplans(expr) || contain_volatile_functions(expr))
			return NIL;
		keys = lappend(keys, expr);
	}
	return keys;
}

static List *
group_keys(PlannerInfo *root)
{
	return clause_keys(root, root->processed_groupClause);
}

/* The core's aggregate paths of the list with this strategy and split. */
static List *
aggregate_templates(const List *pathlist, AggStrategy strategy, AggSplit aggsplit)
{
	List	   *templates = NIL;

	foreach_ptr(Path, path, pathlist)
	{
		if (IsA(path, AggPath) &&
			((AggPath *) path)->aggstrategy == strategy &&
			((AggPath *) path)->aggsplit == aggsplit)
			templates = lappend(templates, path);
	}
	return templates;
}

/*
 * The node's own cost of grouping: its batch kernels hash the keys and
 * look the groups up for a fraction of the core's cpu_operator_cost a key
 * and a row, fold its own aggregates and sum states for such a fraction
 * too, and call a generic aggregate's transition function as the core
 * does. A sum state is five words of the group's record, not the core's
 * estimate of its transition state (128 bytes for numeric's). Past seven
 * eighths of hash_mem the rows of the groups that do not fit go to the
 * partitions of a level of rows, ROWS_PARTS by the level plan, and are
 * read back once per level, their columns written in blocks sequentially,
 * without the core's penalty for random writes. The
 * shares, tessera.agg_key_share and tessera.agg_kernel_share (0.25 each),
 * are measured: grouping alone took 0.30 of the core's time, with generic
 * aggregates 0.41 to 0.53, spilling 0.40 to 0.63 (plan 5.13).
 */

/*
 * The aggregates a node keeps and the aggregates of its scan tuple: each
 * an output, the value of a state or derived from the states of an
 * expression it folds (fold_aggregates), with the derivations' states and
 * constants as the plan data carries them (AGG_DERIVED_*).
 */
typedef struct AggFold
{
	List	   *states;
	List	   *outputs;
	List	   *derived;
	List	   *constants;
	List	   *checks;
	List	   *probes;
	int			nfolded;
} AggFold;

#ifdef HAVE_INT128

/*
 * The bounds of a form of numeric, within a margin of the format: the
 * digits before the point of any value of a step or of a derived sum,
 * and their scale, past which numeric_mul would round (NUMERIC_DSCALE_MAX,
 * 16 383). The digits of a count of rows, below 2^63.
 */
#define FOLD_NUMERIC_DIGITS 100000
#define FOLD_NUMERIC_SCALE 1000
#define FOLD_COUNT_DIGITS 19

/*
 * A folded form of an argument, a·e + b over an expression e of the
 * input: the steps from e out, each the core's own function with a
 * non-NULL constant on its other side. While its type is an integer, a
 * and b and the range of its values over every e of e's type where no
 * step failed, safe when no step can fail on that range; from the step
 * that makes it numeric, a and b as the core's numeric functions make
 * them, their scales kept, and bounds of its values' digits and scale.
 * The digits and the scale of e itself bound its sum.
 */
typedef struct FoldForm
{
	Node	   *e;
	int128		a;
	int128		b;
	int128		low;
	int128		high;
	bool		numeric;
	Datum		a_numeric;
	Datum		b_numeric;
	int			digits;
	int			scale;
	int			e_digits;
	int			e_scale;
	bool		safe;
	bool		steps;
} FoldForm;

typedef enum FoldStep
{
	FOLD_NONE,
	FOLD_ADD,
	FOLD_SUB,
	FOLD_MUL,
	FOLD_NEGATE,
	FOLD_WIDEN,
	FOLD_TO_NUMERIC
} FoldStep;

/* The step a function is, by its identity: an extension's is none. */
static FoldStep
fold_step(Oid function)
{
	switch (function)
	{
		case F_INT2PL:
		case F_INT4PL:
		case F_INT8PL:
		case F_INT24PL:
		case F_INT42PL:
		case F_INT28PL:
		case F_INT82PL:
		case F_INT48PL:
		case F_INT84PL:
		case F_NUMERIC_ADD:
			return FOLD_ADD;
		case F_INT2MI:
		case F_INT4MI:
		case F_INT8MI:
		case F_INT24MI:
		case F_INT42MI:
		case F_INT28MI:
		case F_INT82MI:
		case F_INT48MI:
		case F_INT84MI:
		case F_NUMERIC_SUB:
			return FOLD_SUB;
		case F_INT2MUL:
		case F_INT4MUL:
		case F_INT8MUL:
		case F_INT24MUL:
		case F_INT42MUL:
		case F_INT28MUL:
		case F_INT82MUL:
		case F_INT48MUL:
		case F_INT84MUL:
		case F_NUMERIC_MUL:
			return FOLD_MUL;
		case F_INT2UM:
		case F_INT4UM:
		case F_INT8UM:
		case F_NUMERIC_UMINUS:
			return FOLD_NEGATE;
		case F_INT4_INT2:
		case F_INT8_INT2:
		case F_INT8_INT4:
			return FOLD_WIDEN;
		case F_NUMERIC_INT2:
		case F_NUMERIC_INT4:
		case F_NUMERIC_INT8:
			return FOLD_TO_NUMERIC;
		default:
			return FOLD_NONE;
	}
}

/* An integer type's range and digits; false for another type. */
static bool
integer_range(Oid type, int128 *low, int128 *high, int *digits)
{
	switch (type)
	{
		case INT2OID:
			*low = PG_INT16_MIN;
			*high = PG_INT16_MAX;
			*digits = 5;
			return true;
		case INT4OID:
			*low = PG_INT32_MIN;
			*high = PG_INT32_MAX;
			*digits = 10;
			return true;
		case INT8OID:
			*low = PG_INT64_MIN;
			*high = PG_INT64_MAX;
			*digits = 19;
			return true;
		default:
			return false;
	}
}

/* An integer step's constant: a non-NULL Const of an integer type. */
static bool
integer_constant(Node *node, int128 *value)
{
	Const	   *constant = (Const *) node;

	if (!IsA(node, Const) || constant->constisnull)
		return false;
	switch (constant->consttype)
	{
		case INT2OID:
			*value = DatumGetInt16(constant->constvalue);
			return true;
		case INT4OID:
			*value = DatumGetInt32(constant->constvalue);
			return true;
		case INT8OID:
			*value = DatumGetInt64(constant->constvalue);
			return true;
		default:
			return false;
	}
}

/* A numeric step's constant: a non-NULL, finite Const of numeric. */
static bool
numeric_constant(Node *node, Datum *value)
{
	Const	   *constant = (Const *) node;

	if (!IsA(node, Const) || constant->constisnull || constant->consttype != NUMERICOID ||
		numeric_is_nan(DatumGetNumeric(constant->constvalue)) ||
		numeric_is_inf(DatumGetNumeric(constant->constvalue)))
		return false;
	*value = constant->constvalue;
	return true;
}

/* The digits before the point, at least one, and after it of a finite numeric. */
static void
numeric_digits(Datum value, int *before, int *after)
{
	const char *text = DatumGetCString(DirectFunctionCall1(numeric_out, value));
	const char *point;

	if (*text == '-')
		text++;
	point = strchr(text, '.');
	*before = point != NULL ? (int) (point - text) : (int) strlen(text);
	*after = point != NULL ? (int) strlen(point + 1) : 0;
}

/*
 * A numeric column of a plain table or a materialized view of a declared
 * precision, and its values' digits before the point and scale: the table
 * keeps every value it stores within its typmod, finite or NaN, which a
 * function in FROM, a foreign table or a partition of another kind does
 * not promise. The typmod's fields as numeric.c packs them.
 */
static bool
numeric_column(PlannerInfo *root, Node *node, int *digits, int *scale)
{
	Var		   *var = (Var *) node;
	RangeTblEntry *rte;
	int			precision;
	int			declared;

	if (!IsA(node, Var) || var->vartype != NUMERICOID || var->varlevelsup != 0 ||
		var->varattno <= 0 || var->varno < 1 || var->varno >= root->simple_rel_array_size ||
		var->vartypmod < (int32) VARHDRSZ)
		return false;
	rte = root->simple_rte_array[var->varno];
	if (rte == NULL || rte->rtekind != RTE_RELATION || rte->inh ||
		(rte->relkind != RELKIND_RELATION && rte->relkind != RELKIND_MATVIEW))
		return false;
	precision = ((var->vartypmod - VARHDRSZ) >> 16) & 0xffff;
	declared = (((var->vartypmod - VARHDRSZ) & 0x7ff) ^ 1024) - 1024;
	*digits = Max(precision - declared, 1);
	*scale = Max(declared, 0);
	return true;
}

/*
 * The folded form of an expression; false where it is none: e, where the
 * steps stop, no constant and with no volatile function, of an integer
 * type or a numeric column (numeric_column); a and b of an integer form
 * within 128 bits, a numeric form within the bounds. Each integer step
 * is monotone in e and fails only past its type's range, which the range
 * of the next step's values leaves out; a numeric step within the bounds
 * neither fails nor rounds, and passes NaN on.
 */
static bool
fold_form(PlannerInfo *root, Node *node, FoldForm *form)
{
	FoldStep	step = FOLD_NONE;
	List	   *args = NIL;
	Node	   *inner = NULL;
	bool		numeric = exprType(node) == NUMERICOID;
	int128		constant = 0;
	Datum		decimal = (Datum) 0;
	bool		constant_first = false;
	int128		low;
	int128		high;
	int128		type_low;
	int128		type_high;
	int			digits;

	if (IsA(node, OpExpr))
	{
		OpExpr	   *op = (OpExpr *) node;

		step = fold_step(OidIsValid(op->opfuncid) ? op->opfuncid : get_opcode(op->opno));
		args = op->args;
	}
	else if (IsA(node, FuncExpr))
	{
		step = fold_step(((FuncExpr *) node)->funcid);
		args = ((FuncExpr *) node)->args;
	}
	if (step == FOLD_NEGATE || step == FOLD_WIDEN || step == FOLD_TO_NUMERIC)
		inner = linitial(args);
	else if (step != FOLD_NONE &&
			 (numeric ? numeric_constant(linitial(args), &decimal) :
			  integer_constant(linitial(args), &constant)))
	{
		inner = lsecond(args);
		constant_first = true;
	}
	else if (step != FOLD_NONE &&
			 (numeric ? numeric_constant(lsecond(args), &decimal) :
			  integer_constant(lsecond(args), &constant)))
		inner = linitial(args);
	/* e: an expression no step of a constant makes. */
	if (inner == NULL)
	{
		if (IsA(node, Const) || contain_volatile_functions(node))
			return false;
		form->e = node;
		form->a = 1;
		form->b = 0;
		form->safe = true;
		form->steps = false;
		form->numeric = false;
		form->e_scale = 0;
		if (integer_range(exprType(node), &form->low, &form->high, &form->e_digits))
			return true;
		if (!numeric_column(root, node, &form->e_digits, &form->e_scale))
			return false;
		form->numeric = true;
		form->a_numeric = NumericGetDatum(int64_to_numeric(1));
		form->b_numeric = NumericGetDatum(int64_to_numeric(0));
		form->digits = form->e_digits;
		form->scale = form->e_scale;
		return true;
	}
	if (!fold_form(root, inner, form) || form->numeric != (numeric && step != FOLD_TO_NUMERIC))
		return false;
	form->steps = true;
	/* An integer form made numeric: a and b as numerics, its values an integer type's. */
	if (step == FOLD_TO_NUMERIC)
	{
		form->numeric = true;
		form->a_numeric = agg_int128_numeric(form->a, 0);
		form->b_numeric = agg_int128_numeric(form->b, 0);
		if (!integer_range(exprType(inner), &type_low, &type_high, &form->digits))
			return false;
		form->scale = 0;
		return true;
	}
	if (numeric)
	{
		int			before = 0;
		int			after = 0;

		if (step != FOLD_NEGATE)
			numeric_digits(decimal, &before, &after);
		/*
		 * The bounds first: a and b have no more digits than the step's
		 * values, and an arithmetic past the format would raise an error
		 * the query need not.
		 */
		if (step == FOLD_MUL)
		{
			form->digits += before;
			form->scale += after;
		}
		else if (step != FOLD_NEGATE)
		{
			form->digits = Max(form->digits, before) + 1;
			form->scale = Max(form->scale, after);
		}
		if (form->digits > FOLD_NUMERIC_DIGITS || form->scale > FOLD_NUMERIC_SCALE)
			return false;
		switch (step)
		{
			case FOLD_ADD:
			case FOLD_SUB:
				if (step == FOLD_SUB && constant_first)
				{
					form->a_numeric = DirectFunctionCall1(numeric_uminus, form->a_numeric);
					form->b_numeric = DirectFunctionCall2(numeric_sub, decimal, form->b_numeric);
				}
				else
					form->b_numeric = DirectFunctionCall2(step == FOLD_ADD ? numeric_add : numeric_sub,
														  form->b_numeric, decimal);
				break;
			case FOLD_MUL:
				form->a_numeric = DirectFunctionCall2(numeric_mul, form->a_numeric, decimal);
				form->b_numeric = DirectFunctionCall2(numeric_mul, form->b_numeric, decimal);
				break;
			case FOLD_NEGATE:
				form->a_numeric = DirectFunctionCall1(numeric_uminus, form->a_numeric);
				form->b_numeric = DirectFunctionCall1(numeric_uminus, form->b_numeric);
				break;
			default:
				return false;
		}
		return true;
	}
	low = form->low;
	high = form->high;
	switch (step)
	{
		case FOLD_ADD:
			if (__builtin_add_overflow(form->b, constant, &form->b))
				return false;
			low += constant;
			high += constant;
			break;
		case FOLD_SUB:
			if (constant_first)
			{
				if (__builtin_sub_overflow((int128) 0, form->a, &form->a) ||
					__builtin_sub_overflow(constant, form->b, &form->b))
					return false;
				low = constant - form->high;
				high = constant - form->low;
			}
			else
			{
				if (__builtin_sub_overflow(form->b, constant, &form->b))
					return false;
				low -= constant;
				high -= constant;
			}
			break;
		case FOLD_MUL:
			if (__builtin_mul_overflow(form->a, constant, &form->a) ||
				__builtin_mul_overflow(form->b, constant, &form->b))
				return false;
			low = (constant >= 0 ? form->low : form->high) * constant;
			high = (constant >= 0 ? form->high : form->low) * constant;
			break;
		case FOLD_NEGATE:
			if (__builtin_sub_overflow((int128) 0, form->a, &form->a) ||
				__builtin_sub_overflow((int128) 0, form->b, &form->b))
				return false;
			low = -form->high;
			high = -form->low;
			break;
		default:
			break;
	}
	/* The values past the step's type fail; the rest go on. */
	if (!integer_range(exprType(node), &type_low, &type_high, &digits))
		return false;
	if (low < type_low || high > type_high)
		form->safe = false;
	form->low = Max(low, type_low);
	form->high = Min(high, type_high);
	return form->low <= form->high;
}

/*
 * The form of a constant c, 0·e + c without an e: of an integer type or a
 * finite numeric, not NULL; its aggregates read no column.
 */
static bool
fold_constant_form(Node *node, FoldForm *form)
{
	int			digits;

	*form = (FoldForm)
	{
		0
	};
	form->safe = true;
	form->steps = true;
	if (integer_constant(node, &form->b))
	{
		form->low = form->high = form->b;
		return integer_range(exprType(node), &form->low, &form->high, &digits);
	}
	if (!numeric_constant(node, &form->b_numeric))
		return false;
	form->numeric = true;
	form->a_numeric = NumericGetDatum(int64_to_numeric(0));
	numeric_digits(form->b_numeric, &form->digits, &form->scale);
	return true;
}

/* The sign of a form's factor: of its int128 a, or of its numeric one. */
static int
fold_sign(const FoldForm *form)
{
	if (form->numeric)
		return DatumGetInt32(DirectFunctionCall2(numeric_cmp, form->a_numeric,
												 NumericGetDatum(int64_to_numeric(0))));
	return form->a > 0 ? 1 : form->a < 0 ? -1 : 0;
}

/*
 * Whether a numeric form's derived sum a * S + b * n stays within the
 * bounds, for 2^63 rows at most, S of e's digits and scale.
 */
static bool
fold_sum_bounded(const FoldForm *form)
{
	int			a_before;
	int			a_after;
	int			b_before;
	int			b_after;

	if (!form->numeric)
		return true;
	numeric_digits(form->a_numeric, &a_before, &a_after);
	numeric_digits(form->b_numeric, &b_before, &b_after);
	return a_before + form->e_digits + FOLD_COUNT_DIGITS + 1 <= FOLD_NUMERIC_DIGITS &&
		b_before + FOLD_COUNT_DIGITS + 1 <= FOLD_NUMERIC_DIGITS &&
		a_after + form->e_scale <= FOLD_NUMERIC_SCALE && b_after <= FOLD_NUMERIC_SCALE;
}

/* The extreme of e that gives min or max of a form, by the sign of its factor. */
static bool
fold_extreme_min(const Aggref *agg, const FoldForm *form)
{
	bool		least = agg->aggfnoid == F_MIN_INT2 || agg->aggfnoid == F_MIN_INT4 ||
		agg->aggfnoid == F_MIN_INT8 || agg->aggfnoid == F_MIN_NUMERIC;

	return fold_sign(form) >= 0 ? least : !least;
}

/* Whether an aggregate is min or max of a fold. */
static bool
fold_extreme(const Aggref *agg)
{
	switch (agg->aggfnoid)
	{
		case F_MIN_INT2:
		case F_MIN_INT4:
		case F_MIN_INT8:
		case F_MIN_NUMERIC:
		case F_MAX_INT2:
		case F_MAX_INT4:
		case F_MAX_INT8:
		case F_MAX_NUMERIC:
			return true;
		default:
			return false;
	}
}

/*
 * Whether an aggregate of the scan tuple may fold, and its form: sum,
 * avg, count, min and max of one argument, a plain call without DISTINCT,
 * its FILTER without a volatile function, its argument a form of a
 * smallint, an integer, a bigint or a numeric column e, with the kernels
 * module, whose batch aggregates and sum states the fold's states are;
 * min and max of a numeric e only with a positive factor, as NaN, the
 * greatest of numerics, would not follow a negative one.
 */
static bool
fold_candidate(PlannerInfo *root, const Aggref *agg, FoldForm *form)
{
	switch (agg->aggfnoid)
	{
		case F_SUM_INT2:
		case F_SUM_INT4:
		case F_SUM_INT8:
		case F_AVG_INT2:
		case F_AVG_INT4:
		case F_AVG_INT8:
		case F_COUNT_ANY:
		case F_MIN_INT2:
		case F_MIN_INT4:
		case F_MIN_INT8:
		case F_MAX_INT2:
		case F_MAX_INT4:
		case F_MAX_INT8:
		case F_SUM_NUMERIC:
		case F_AVG_NUMERIC:
		case F_MIN_NUMERIC:
		case F_MAX_NUMERIC:
			break;
		default:
			return false;
	}
	/* The states a fold makes are the kernels' batch aggregates and sum states. */
	if (tess_runtime_kernels() == NULL ||
		agg->aggsplit != AGGSPLIT_SIMPLE || list_length(agg->args) != 1 ||
		contain_volatile_functions((Node *) agg->aggfilter) ||
		(agg->aggdistinct != NIL && (fold_extreme(agg) || list_length(agg->aggdistinct) != 1)))
		return false;
	/* A constant, without DISTINCT: from the count of the rows read. */
	if (IsA(aggregate_argument(agg), Const))
		return agg->aggdistinct == NIL &&
			fold_constant_form(aggregate_argument(agg), form) && fold_sum_bounded(form);
	if (!fold_form(root, aggregate_argument(agg), form) || !fold_sum_bounded(form))
		return false;
	/*
	 * DISTINCT: a factor not 0 maps the distinct values of e one to one, NaN
	 * to NaN, so the forms' distinct values are e's; of a numeric e only
	 * their count, as which of equal values comes first sets a sum's scale.
	 */
	if (agg->aggdistinct != NIL &&
		(fold_sign(form) == 0 ||
		 (exprType(form->e) == NUMERICOID && agg->aggfnoid != F_COUNT_ANY)))
		return false;
	return exprType(form->e) != NUMERICOID || !fold_extreme(agg) || fold_sign(form) > 0;
}

/*
 * A state the planner makes of e: sum of integer or count("any"), int8
 * both, avg of bigint or numeric, whose sum state the node keeps, or an
 * extreme of an integer, a bigint or a numeric, of its type; count(*)
 * without an argument.
 */
static Aggref *
fold_state(Oid aggfnoid, Node *argument, Expr *filter, bool distinct)
{
	Aggref	   *agg = makeNode(Aggref);
	bool		sum_state = aggfnoid == F_AVG_INT4 || aggfnoid == F_AVG_INT8 ||
		aggfnoid == F_AVG_NUMERIC;
	TargetEntry *entry;

	/* count(*) of the rows a FILTER keeps. */
	if (argument == NULL)
	{
		agg->aggfnoid = F_COUNT_;
		agg->aggtype = INT8OID;
		agg->aggtranstype = INT8OID;
		agg->aggstar = true;
		agg->aggfilter = copyObject(filter);
		agg->aggkind = AGGKIND_NORMAL;
		agg->aggsplit = AGGSPLIT_SIMPLE;
		agg->aggno = -1;
		agg->aggtransno = -1;
		agg->location = -1;
		return agg;
	}
	entry = makeTargetEntry((Expr *) copyObject(argument), 1, NULL, false);

	agg->aggfnoid = aggfnoid;
	agg->aggtype = sum_state || aggfnoid == F_MIN_NUMERIC || aggfnoid == F_MAX_NUMERIC ?
		NUMERICOID : aggfnoid == F_MIN_INT4 || aggfnoid == F_MAX_INT4 ? INT4OID : INT8OID;
	agg->aggtranstype = aggfnoid == F_AVG_INT4 ? INT8ARRAYOID :
		sum_state ? INTERNALOID : agg->aggtype;
	agg->aggargtypes = list_make1_oid(exprType(argument));
	/* DISTINCT over the argument, by its type's equality, as the parser sorts it. */
	if (distinct)
	{
		SortGroupClause *clause = makeNode(SortGroupClause);

		get_sort_group_operators(exprType(argument), true, true, false, &clause->sortop,
								 &clause->eqop, NULL, &clause->hashable);
		clause->tleSortGroupRef = entry->ressortgroupref = 1;
		agg->aggdistinct = list_make1(clause);
	}
	agg->args = list_make1(entry);
	agg->aggfilter = copyObject(filter);
	agg->aggkind = AGGKIND_NORMAL;
	agg->aggsplit = AGGSPLIT_SIMPLE;
	agg->aggno = -1;
	agg->aggtransno = -1;
	agg->location = -1;
	return agg;
}

/* A form's argument with its e the one column of the inner tuple, a probe of its check. */
static Node *
fold_probe(Node *node, Node *e)
{
	if (node == e)
		return (Node *) makeVar(INNER_VAR, 1, exprType(e), exprTypmod(e), exprCollation(e), 0);
	return expression_tree_mutator(node, fold_probe, e);
}

/*
 * A derivation's constant: of an integer form an int8, or past one a
 * numeric; of a numeric form the numeric as its steps made it.
 */
static Const *
fold_constant(const FoldForm *form, bool factor)
{
	int128		value = factor ? form->a : form->b;

	if (form->numeric)
		return makeConst(NUMERICOID, -1, InvalidOid, -1,
						 factor ? form->a_numeric : form->b_numeric, false, false);
	if (value >= PG_INT64_MIN && value <= PG_INT64_MAX)
		return makeConst(INT8OID, -1, InvalidOid, sizeof(int64),
						 Int64GetDatum((int64) value), false, true);
	return makeConst(NUMERICOID, -1, InvalidOid, -1, agg_int128_numeric(value, 0),
					 false, false);
}

#endif							/* HAVE_INT128 */

#ifdef HAVE_INT128
/*
 * The states a group of e keeps: one set for DISTINCT, the sum state of a
 * bigint's avg, sum and count of the others, and its extremes.
 */
static int
fold_states(Node *e, bool distinct, bool sum, bool least, bool greatest)
{
	if (distinct)
		return 1;
	return (sum ? (exprType(e) == INT8OID ? 1 : 2) : 0) + (least ? 1 : 0) + (greatest ? 1 : 0);
}
#endif

/*
 * The states and outputs of the aggregates of a scan tuple after its keys.
 * Where fold, the aggregates of one e and one FILTER, at least two, one
 * of a form other than e itself, and no fewer than the states they would
 * share, keep the states of e they need over
 * the rows they read, from which each of them is derived, those of e
 * itself among them. Sum, avg and count need e's sum and count: for a
 * smallint or an integer e, sum(e) and count(e), words, e cast to
 * integer for a smallint, as below 2^32 rows the sum of e holds every sum
 * their forms' own sums hold; for a bigint e, the sum state of avg(e),
 * which holds the count too. min and max need min(e), or max(e) where the
 * factor of e is negative, words of e's type, integer for a smallint.
 * DISTINCT aggregates of e keep one set of pairs for them all, of
 * avg(DISTINCT e) of an integer or count(DISTINCT e) of a numeric. The
 * aggregates of constants under one FILTER, one or more, keep count(*).
 * Where a form can pass a step's type, the group has a check of errors,
 * the extremes of e and the arguments of those forms as its probes.
 * Every other aggregate is a state of its own, as without fold.
 */
static void
fold_aggregates(PlannerInfo *root, List *tlist, int nkeys, bool fold, AggFold *result)
{
	int			naggs = list_length(tlist) - nkeys;
	int		   *group = palloc_array(int, Max(naggs, 1));
#ifdef HAVE_INT128
	int		   *members = palloc0_array(int, Max(naggs, 1));
	bool	   *steps = palloc0_array(bool, Max(naggs, 1));
	/* A group of constants, which folds alone too. */
	bool	   *alone = palloc0_array(bool, Max(naggs, 1));
	bool	   *unsafe = palloc0_array(bool, Max(naggs, 1));
	/* Of each group's first: the states it needs, and where they went. */
	bool	   *need_sum = palloc0_array(bool, Max(naggs, 1));
	bool	   *need_min = palloc0_array(bool, Max(naggs, 1));
	bool	   *need_max = palloc0_array(bool, Max(naggs, 1));
	int		   *first = palloc_array(int, Max(naggs, 1));
	int		   *check = palloc_array(int, Max(naggs, 1));
	FoldForm   *forms = palloc0_array(FoldForm, Max(naggs, 1));
#endif

	*result = (AggFold)
	{
		0
	};
	for (int i = 0; i < naggs; i++)
	{
#ifdef HAVE_INT128
		Aggref	   *agg = (Aggref *) list_nth_node(TargetEntry, tlist, nkeys + i)->expr;

		first[i] = -1;
		check[i] = -1;
#endif
		group[i] = -1;
#ifdef HAVE_INT128
		if (!fold || !IsA(agg, Aggref) || !fold_candidate(root, agg, &forms[i]))
			continue;
		group[i] = i;
		for (int j = 0; j < i; j++)
		{
			Aggref	   *other = (Aggref *) list_nth_node(TargetEntry, tlist, nkeys + j)->expr;

			if (group[j] == j && equal(forms[j].e, forms[i].e) &&
				equal(other->aggfilter, agg->aggfilter) &&
				(other->aggdistinct != NIL) == (agg->aggdistinct != NIL))
			{
				group[i] = j;
				break;
			}
		}
		members[group[i]]++;
		alone[group[i]] = forms[i].e == NULL;
		steps[group[i]] |= forms[i].steps;
		unsafe[group[i]] |= !forms[i].safe;
		if (fold_extreme(agg))
		{
			need_min[group[i]] |= fold_extreme_min(agg, &forms[i]);
			need_max[group[i]] |= !fold_extreme_min(agg, &forms[i]);
		}
		else
			need_sum[group[i]] = true;
#endif
	}
	for (int i = 0; i < naggs; i++)
	{
		Aggref	   *agg = (Aggref *) list_nth_node(TargetEntry, tlist, nkeys + i)->expr;
		int			j = group[i];
		bool		own = j < 0;

#ifdef HAVE_INT128
		/* Aggregates of constants fold alone too; the others as they save states. */
		own = own || (!alone[j] &&
					  (members[j] < 2 || !steps[j] ||
					   fold_states(forms[j].e,
								   ((Aggref *) list_nth_node(TargetEntry, tlist,
															 nkeys + j)->expr)->aggdistinct != NIL,
								   need_sum[j], need_min[j], need_max[j]) > members[j]));
#endif
		if (own)
		{
			result->outputs = lappend_int(result->outputs, list_length(result->states));
			result->states = lappend(result->states, copyObject(agg));
			continue;
		}
#ifdef HAVE_INT128
		/* The group's states, at its first: sum and count, min, max, as it needs them. */
		if (first[j] < 0)
		{
			Node	   *e = forms[j].e;
			Oid			type = e != NULL ? exprType(e) : InvalidOid;
			Node	   *word = type != INT2OID ? e :
				(Node *) makeFuncExpr(F_INT4_INT2, INT4OID, list_make1(copyObject(e)),
									  InvalidOid, InvalidOid, COERCE_IMPLICIT_CAST);

			first[j] = list_length(result->states);
			/* Of constants: count(*) under their FILTER. */
			if (e == NULL)
			{
				Aggref	   *rows = fold_state(F_COUNT_, NULL, agg->aggfilter, false);

				result->states = lappend(result->states, rows);
			}
			/*
			 * DISTINCT, one set of pairs for them all: the sum state of
			 * avg(DISTINCT e) of an integer, which holds the count of the
			 * distinct values too, or count(DISTINCT e) of a numeric.
			 */
			else if (agg->aggdistinct != NIL)
				result->states = lappend(result->states,
										 fold_state(type == NUMERICOID ? F_COUNT_ANY :
													type == INT8OID ? F_AVG_INT8 : F_AVG_INT4,
													word, agg->aggfilter, true));
			else if (need_sum[j] && type == INT8OID)
				result->states = lappend(result->states,
										 fold_state(F_AVG_INT8, e, agg->aggfilter, false));
			else if (need_sum[j])
			{
				result->states = lappend(result->states,
										 fold_state(type == NUMERICOID ? F_AVG_NUMERIC : F_SUM_INT4,
													word, agg->aggfilter, false));
				result->states = lappend(result->states,
										 fold_state(F_COUNT_ANY, word, agg->aggfilter, false));
			}
			if (need_min[j] && e != NULL)
				result->states = lappend(result->states,
										 fold_state(type == NUMERICOID ? F_MIN_NUMERIC :
													type == INT8OID ? F_MIN_INT8 : F_MIN_INT4,
													word, agg->aggfilter, false));
			if (need_max[j] && e != NULL)
				result->states = lappend(result->states,
										 fold_state(type == NUMERICOID ? F_MAX_NUMERIC :
													type == INT8OID ? F_MAX_INT8 : F_MAX_INT4,
													word, agg->aggfilter, false));
			if (unsafe[j])
			{
				check[j] = list_length(result->probes);
				result->checks = lappend_int(result->checks, first[j]);
				result->checks = lappend_int(result->checks, (int) exprType(e));
				result->probes = lappend(result->probes, NIL);
			}
		}
		if (!forms[i].safe)
		{
			ListCell   *probes = list_nth_cell(result->probes, check[j]);

			lfirst(probes) = lappend((List *) lfirst(probes),
									 fold_probe(aggregate_argument(agg), forms[i].e));
		}
		result->outputs = lappend_int(result->outputs,
									  -1 - list_length(result->derived) / AGG_DERIVED_STATES);
		if (forms[j].e == NULL)
		{
			/* A constant's: the count of rows, as the sum's count and its own. */
			result->derived = lappend_int(result->derived, first[j]);
			result->derived = lappend_int(result->derived, first[j]);
		}
		else if (fold_extreme(agg))
		{
			/* After the sum's states, if any: min(e) first. */
			int			extreme = first[j] +
				(need_sum[j] ? (exprType(forms[j].e) == INT8OID ? 1 : 2) : 0) +
				(fold_extreme_min(agg, &forms[i]) ? 0 : need_min[j] ? 1 : 0);

			result->derived = lappend_int(result->derived, extreme);
			result->derived = lappend_int(result->derived, -1);
		}
		else if (agg->aggdistinct != NIL)
		{
			/* The distinct values' sum state, or their count of numeric alone. */
			result->derived = lappend_int(result->derived, first[j]);
			result->derived = lappend_int(result->derived,
										  exprType(forms[j].e) == NUMERICOID ? first[j] : -1);
		}
		else
		{
			result->derived = lappend_int(result->derived, first[j]);
			result->derived = lappend_int(result->derived,
										  exprType(forms[j].e) == INT8OID ? -1 : first[j] + 1);
		}
		result->constants = lappend(result->constants, fold_constant(&forms[i], true));
		result->constants = lappend(result->constants, fold_constant(&forms[i], false));
		result->nfolded++;
#endif
	}
}

/* The keys of a scan tuple and the states of its fold, as a target list. */
static List *
fold_target_list(List *tlist, int nkeys, const AggFold *fold)
{
	List	   *result = list_copy_head(tlist, nkeys);

	foreach_ptr(Expr, state, fold->states)
		result = lappend(result, makeTargetEntry(state, list_length(result) + 1, NULL, false));
	return result;
}

/*
 * The costs of a node's aggregates where it folds some, in place of the
 * core's costs of the query's (get_agg_clause_costs), which the folded
 * aggregates' own arguments make: each state's transition function and
 * the evaluation of its argument and FILTER a row, its final function a
 * group, the space of its transition value as the core counts it, and a
 * derivation of each folded aggregate a group.
 */
static void
aggregate_costs(PlannerInfo *root, AggSplit split, List *state_tlist, int nfolded,
				AggClauseCosts *costs)
{
	MemSet(costs, 0, sizeof(*costs));
	if (nfolded == 0)
	{
		if (root->parse->hasAggs)
			get_agg_clause_costs(root, split, costs);
		return;
	}
	foreach_node(TargetEntry, entry, state_tlist)
	{
		Aggref	   *agg = (Aggref *) entry->expr;
		HeapTuple	tuple;
		Form_pg_aggregate form;
		QualCost	eval;
		int16		len;
		bool		byval;

		if (!IsA(agg, Aggref))
			continue;
		tuple = SearchSysCache1(AGGFNOID, ObjectIdGetDatum(agg->aggfnoid));
		if (!HeapTupleIsValid(tuple))
			elog(ERROR, "cache lookup failed for aggregate %u", agg->aggfnoid);
		form = (Form_pg_aggregate) GETSTRUCT(tuple);
		add_function_cost(root, form->aggtransfn, NULL, &costs->transCost);
		if (OidIsValid(form->aggfinalfn))
			add_function_cost(root, form->aggfinalfn, NULL, &costs->finalCost);
		get_typlenbyval(agg->aggtranstype, &len, &byval);
		if (!byval)
			costs->transitionSpace +=
				MAXALIGN(form->aggtransspace > 0 ? form->aggtransspace :
						 get_typavgwidth(agg->aggtranstype, -1)) + 2 * sizeof(void *);
		else if (agg->aggtranstype == INTERNALOID)
			costs->transitionSpace += form->aggtransspace > 0 ? form->aggtransspace :
				ALLOCSET_DEFAULT_INITSIZE;
		ReleaseSysCache(tuple);
		cost_qual_eval_node(&eval, (Node *) agg->args, root);
		costs->transCost.startup += eval.startup;
		costs->transCost.per_tuple += eval.per_tuple;
		if (agg->aggfilter != NULL)
		{
			cost_qual_eval_node(&eval, (Node *) agg->aggfilter, root);
			costs->transCost.startup += eval.startup;
			costs->transCost.per_tuple += eval.per_tuple;
		}
	}
	costs->finalCost.per_tuple += nfolded * cpu_operator_cost;
}

/*
 * The cost a row of the aggregates' arguments that the kernels do not
 * compute, none when the aggregates combine partial values: the node's
 * projection evaluates them row by row, as the core's aggregate does.
 */
static Cost
rowwise_argument_cost(PlannerInfo *root, List *tlist, AggSplit split)
{
	Cost		cost = 0;

	if (DO_AGGSPLIT_COMBINE(split))
		return 0;
	foreach_node(TargetEntry, entry, tlist)
	{
		if (!IsA(entry->expr, Aggref))
			continue;
		foreach_node(TargetEntry, arg, ((Aggref *) entry->expr)->args)
		{
			QualCost	eval;

			if (tess_expr_supports_value((Node *) arg->expr, 0))
				continue;
			cost_qual_eval_node(&eval, (Node *) arg->expr, root);
			cost += eval.per_tuple;
		}
	}
	return cost;
}

static void
group_cost(PlannerInfo *root, const Path *child, double groups, int nkeys,
		   List *tlist, int nfolded, AggSplit split, const PairsEstimate *pairs,
		   Path *result)
{
	AggClauseCosts costs;
	double		rows = child->rows;
	double		width = 0;
	double		entry = (double) tess_table_record_bytes(nkeys, sizeof(uint64) *
														 AGG_FLAG_WORDS(list_length(tlist) - nkeys));
	double		limit = (double) tess_hash_memory_limit() / 8 * AGG_SPILL_FILL;
	int			naggs = 0;
	int			ncolumns = 0;
	int			nsums = sum_states(tlist);
	Cost		rowwise = rowwise_argument_cost(root, tlist, split);
	Cost		startup;
	Cost		run;

	aggregate_costs(root, split, tlist, nfolded, &costs);
	foreach_node(TargetEntry, entry_node, tlist)
	{
		List	   *exprs = IsA(entry_node->expr, Aggref) ?
			list_copy((List *) ((Aggref *) entry_node->expr)->args) :
			list_make1(makeTargetEntry(entry_node->expr, 1, NULL, false));

		if (IsA(entry_node->expr, Aggref))
			naggs++;
		foreach_node(TargetEntry, arg, exprs)
		{
			Oid			type = exprType((Node *) arg->expr);
			int16		len;
			bool		byval;

			ncolumns++;
			get_typlenbyval(type, &len, &byval);
			width += 8.0 + (byval ? 0 : get_typavgwidth(type, exprTypmod((Node *) arg->expr)));
		}
	}
	entry += 8.0 * naggs + (nsums < 0 ? costs.transitionSpace :
							8.0 * (AGG_SUM_STATE_WORDS - 1) * nsums);
	startup = child->total_cost;
	startup += cpu_operator_cost * tess_agg_key_share * nkeys * rows;
	/*
	 * A key a word does not hold: its type's hash and the dictionary's
	 * lookup a row, tessera.agg_dictionary_share (0.65) past the key's
	 * share, 0.9 of the core's cpu_operator_cost with it, as the set
	 * operations' dictionary measured (plan 5.13, step 5): SELECT DISTINCT
	 * through the dictionary took 0.45 to 0.84 of the core's hashed time
	 * over the same scan with text, varchar and char keys, the same with
	 * numeric (plan 4.21 а).
	 */
	foreach_node(TargetEntry, key, tlist)
	{
		TessTableKeyKind kind;

		if (foreach_current_index(key) >= nkeys)
			break;
		if (!tess_word_key_kind(exprType((Node *) key->expr), &kind))
			startup += cpu_operator_cost * tess_agg_dictionary_share * rows;
	}
	startup += costs.transCost.startup + rowwise * rows +
		(costs.transCost.per_tuple - rowwise) * (nsums < 0 ? 1.0 : tess_agg_kernel_share) * rows;
	/* Groups past hash_mem: their rows to disk and back, once per level. */
	if (groups * entry > limit && ncolumns > 0)
	{
		double		share = 1.0 - limit / (groups * entry);
		double		depth = ceil(log(groups * entry / limit) / log((double) ROWS_PARTS));
		double		spilled = rows * share * Max(depth, 1.0);
		double		pages = spilled * width / BLCKSZ;

		startup += pages * seq_page_cost + spilled * cpu_tuple_cost;
		run = pages * seq_page_cost + spilled * cpu_tuple_cost;
	}
	else
		run = 0;
	/*
	 * DISTINCT pairs past the room the groups leave, set by set: the rows
	 * whose pairs a set lacks once full, written and read back once a
	 * level, three words each, before the first group goes out. A set
	 * whose pairs fit the room writes none, and each set reads its levels
	 * back alone, in the whole room.
	 */
	for (int set = 0; pairs != NULL && set < pairs->sets; set++)
	{
		double		room = limit - pairs->groups;
		double		bytes = pairs->set_pairs[set];

		if (room > 0 && bytes > room)
		{
			double		depth = ceil(log(bytes / room) / log((double) ROWS_PARTS));
			double		spilled = rows * (1.0 - room / bytes) * Max(depth, 1.0);
			double		pages = spilled * 3 * sizeof(uint64) / BLCKSZ;

			startup += 2 * (pages * seq_page_cost + spilled * cpu_tuple_cost);
		}
	}
	startup += costs.finalCost.startup;
	run += costs.finalCost.per_tuple * groups + cpu_tuple_cost * groups;
	result->startup_cost = startup;
	result->total_cost = startup + run;
}

/*
 * The node's own cost of an aggregation without GROUP BY: the child's
 * cost as it is, a batch scan's or a pack's over the core's rows, and the
 * node's own work as a share of the core's transition costs,
 * tessera.agg_kernel_share with its own aggregates and sum states,
 * tessera.agg_generic_share with an aggregate the kernels do not fold
 * (max(text) over 500 000 rows took the node 10.7 ms and the core's
 * Aggregate over the same batch scan 13.4, three aggregates with a text
 * max 19.1 and 21.8, string_agg over 10 000 rows 3.6 and 3.2; plan 8.10);
 * an argument the kernels do not compute costs the core's. Over a pack of
 * nine columns under a window function, the node took 46 ms and the pack
 * 18 where the core's aggregate took 42, its argument seven XORs and an
 * addition, which no kernel computes (plan 4.28). Before plan 8.10 a plain
 * aggregate over a batch scan cost nine tenths of the core's path, child
 * included.
 */
static void
plain_cost(PlannerInfo *root, const Path *child, List *tlist, int nfolded, AggSplit split,
		   Path *result)
{
	AggClauseCosts costs;
	Cost		rowwise = rowwise_argument_cost(root, tlist, split);
	double		share = sum_states(tlist) < 0 ? tess_agg_generic_share : tess_agg_kernel_share;
	double		rows = child->rows;

	aggregate_costs(root, split, tlist, nfolded, &costs);
	result->startup_cost = child->total_cost + rowwise * rows +
		share * (costs.transCost.startup + (costs.transCost.per_tuple - rowwise) * rows +
				 costs.finalCost.startup + costs.finalCost.per_tuple);
	result->total_cost = result->startup_cost + cpu_tuple_cost;
}

/*
 * The node's path in place of the core's aggregate path: the same planner
 * properties and rows, a lower cost, the batch child over the core path's
 * input, the grouping expressions and the aggregates it computes; the
 * groups spill past hash_mem as the core's do. NULL when the input cannot
 * be read in batches or lacks a column. The private data: the keys, the
 * groups expected, the set operation's command (-1), the path's flags
 * (AGG_PATH_*), each key's equality, and the kB of the groups the planner
 * expects where DISTINCT pairs go to disk (pairs), 0 elsewhere.
 */
static CustomPath *
make_agg_path(PlannerInfo *root, const AggPath *agg, List *tlist, int nkeys, int flags,
			  const PairsEstimate *pairs)
{
	TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
	Path	   *child;
	Path		template;
	AggFold		fold;
	List	   *states;

	/* The states are those the plan keeps (agg_plan): the costs are theirs. */
	fold_aggregates(root, tlist, nkeys, (flags & (AGG_PATH_PARTIAL | AGG_PATH_FINALIZE)) == 0,
					&fold);
	states = fold_target_list(tlist, nkeys, &fold);
	child = agg->subpath;
	/* A sort the core put below for its sorted grouping: hashing needs none. */
	while (IsA(child, SortPath) || IsA(child, IncrementalSortPath))
		child = ((SortPath *) child)->subpath;
	child = tess_batch_input_path(root, child);
	if (child == NULL ||
		!((flags & AGG_PATH_FINALIZE) ? partials_available(tlist, child) :
		  arguments_available(states, child)))
		return NULL;
	template = agg->path;
	/*
	 * Grouping and a plain aggregate cost the node's own over the child's
	 * cost; a final one the core's: its work is a row a participant.
	 */
	if (nkeys > 0)
		group_cost(root, child, agg->path.rows, nkeys, states, fold.nfolded, agg->aggsplit,
				   pairs, &template);
	else if ((flags & AGG_PATH_FINALIZE) == 0)
		plain_cost(root, child, states, fold.nfolded, agg->aggsplit, &template);
	/* The groups come in no order, whatever order the core's had. */
	template.pathkeys = NIL;
	config.template_path = &template;
	config.methods = &agg_path_methods;
	config.node = &tess_agg_node;
	config.children = list_make1(child);
	config.expressions = tlist;
	config.node_data = (Node *) list_make4_int(nkeys,
											   (int) Min(agg->path.rows,
														 (double) PG_INT32_MAX),
											   -1, flags);
	/*
	 * Each key's equality, for a key a word does not hold (0 for the
	 * others): its type's default one, which one of the grouping clauses
	 * must use, since the clauses may come in another order than the keys.
	 */
	foreach_node(TargetEntry, entry, tlist)
	{
		int			eqop;

		if (foreach_current_index(entry) >= nkeys)
			break;
		if (!key_eqop((Node *) entry->expr, agg->groupClause, &eqop))
			return NULL;
		config.node_data = (Node *) lappend_int((List *) config.node_data, eqop);
	}
	/* The groups' bytes in kB where the pairs go to disk, which the node weighs. */
	config.node_data = (Node *) lappend_int((List *) config.node_data,
											pairs == NULL ? 0 :
											(int) Min(ceil(pairs->groups / 1024),
													  (double) PG_INT32_MAX));
	return tess_path_create(&config);
}

/*
 * The partial relation (UPPERREL_PARTIAL_GROUP_AGG or
 * UPPERREL_PARTIAL_DISTINCT) of an upper one, when the core built partial
 * paths for it; PostgreSQL passes the partially grouped one to no hook.
 */
static RelOptInfo *
partial_upper_rel(PlannerInfo *root, UpperRelationKind kind, RelOptInfo *upper_rel)
{
	foreach_ptr(RelOptInfo, rel, root->upper_rels[kind])
	{
		if (bms_equal(rel->relids, upper_rel->relids))
			return rel->partial_pathlist != NIL ? rel : NULL;
	}
	return NULL;
}

/*
 * Grouping without aggregates, by GROUP BY, DISTINCT or UNION, in every
 * participant under a gather: over partial, a partial hashed path of the
 * core's, the node's partial path over its batch child, which groups for
 * the gather and empties its table early, TessGather over it, and above
 * that the node's grouping of the participants' groups, where the core
 * would merge them row by row; the final grouping has target, or the
 * partial one's without it, the clauses and the path flags given.
 */
static void
add_key_stack_path(PlannerInfo *root, RelOptInfo *partial_rel, RelOptInfo *output_rel,
				   AggPath *partial, List *keys, PathTarget *target, List *clauses,
				   int flags, double groups)
{
	List	   *tlist = add_to_flat_tlist(NIL, keys);
	CustomPath *below;
	CustomPath *path;
	AggPath    *final;
	Path	   *gather;

	if (not_from_groups((Node *) partial->path.pathtarget->exprs, keys))
		return;
	below = make_agg_path(root, partial, tlist, list_length(keys), AGG_PATH_PARTIAL, NULL);
	if (below == NULL || !below->path.parallel_safe || below->path.parallel_workers <= 0)
		return;
	below->path.parallel_aware = true;
	gather = tess_gather_path(root, partial_rel, &below->path);
	if (gather == NULL)
		return;
	final = create_agg_path(root, output_rel, gather,
							target != NULL ? target : partial->path.pathtarget,
							AGG_HASHED, AGGSPLIT_SIMPLE, clauses, NIL, NULL, groups);
	path = make_agg_path(root, final, tlist, list_length(keys), flags, NULL);
	if (path != NULL)
		add_path(output_rel, &path->path);
}

/* The stack over each of the core's partial hashed paths of partial_rel. */
static void
create_key_stack_paths(PlannerInfo *root, RelOptInfo *partial_rel, RelOptInfo *output_rel,
					   AggSplit split, List *keys, PathTarget *target, List *clauses,
					   int flags, double groups)
{
	if (partial_rel == NULL || keys == NIL || groups <= 0)
		return;
	foreach_ptr(AggPath, agg, aggregate_templates(partial_rel->partial_pathlist,
												  AGG_HASHED, split))
		add_key_stack_path(root, partial_rel, output_rel, agg, keys, target, clauses,
						   flags, groups);
}

/*
 * The node in place of the core's partial aggregate under a Gather.
 * PostgreSQL offers extensions no hook for the partially grouped relation
 * and builds the Gather and the Finalize Aggregate before it calls this
 * one, so the node builds the whole stack for each of the core's partial
 * aggregate paths: its own partial path over the batch child, with the
 * partial aggregates as its targets, TessGather over it and the node's
 * final aggregation of final_tlist (the serial path's keys and
 * aggregates) above, whose arguments are the aggregates' partial values
 * and which applies HAVING (plan 4.23, item 4b: over the node's partial
 * aggregate, always the node's final one). Without GROUP BY it merges one
 * row a participant, a generic aggregate's by its combine function; with
 * GROUP BY each participant keeps a table of its own groups, which it
 * merges in batches, the aggregates the node's own and sum states, whose
 * partial values are the node's own format, which the core's Finalize
 * does not read. Only without TessGather (tessera.batch_gather off) do
 * the core's Gather and Finalize Aggregate merge them, and then not sum
 * states. The path is parallel-aware for the counters the node shares;
 * the child divides the work. Without aggregates the node groups above
 * the gather alone (create_key_stack_paths).
 */
static void
create_partial_paths(PlannerInfo *root, RelOptInfo *grouped_rel,
					 GroupPathExtraData *extra, List *keys, List *final_tlist,
					 double groups)
{
	RelOptInfo *partial_rel = partial_upper_rel(root, UPPERREL_PARTIAL_GROUP_AGG, grouped_rel);
	List	   *tlist = keys != NIL ? add_to_flat_tlist(NIL, keys) : NIL;
	AggStrategy strategy = keys != NIL ? AGG_HASHED : AGG_PLAIN;
	int			nsums = 0;
	int			nown = 0;

	if (partial_rel == NULL || extra == NULL || !extra->partial_costs_set ||
		(keys != NIL && groups <= 0))
		return;
	/* A partial aggregation folds nothing: each aggregate is a state. */
	if (!collect_aggregates((Node *) partial_rel->reltarget->exprs, keys, &tlist) ||
		list_length(tlist) - list_length(keys) > AGG_MAX_GROUPED)
		return;
	if (list_length(tlist) == list_length(keys))
	{
		create_key_stack_paths(root, partial_rel, grouped_rel, AGGSPLIT_INITIAL_SERIAL, keys,
							   grouped_rel->reltarget, root->processed_groupClause,
							   AGG_PATH_HAVING, groups);
		return;
	}
	/*
	 * Generic states would go with a table emptied early too: with GROUP BY
	 * the aggregates are the node's own and sum states. Without it, the
	 * numeric and bigint sums go up in the node's own format too.
	 */
	if (keys != NIL)
		nsums = sum_states(tlist);
	else
		nown = own_partials(tlist);
	if (nsums < 0)
		return;
	foreach_ptr(AggPath, agg, aggregate_templates(partial_rel->partial_pathlist,
												  strategy,
												  AGGSPLIT_INITIAL_SERIAL))
	{
		CustomPath *partial = make_agg_path(root, agg, tlist, list_length(keys),
											AGG_PATH_PARTIAL |
											(nsums > 0 || nown > 0 ?
											 AGG_PATH_OWN_STATES : 0), NULL);
		Path	   *gathered;
		CustomPath *path;
		GatherPath *gather;
		AggPath    *final;
		double		rows;

		if (partial == NULL || !partial->path.parallel_safe ||
			partial->path.parallel_workers <= 0)
			continue;
		partial->path.parallel_aware = true;
		/* The node merges the participants' values itself, above TessGather. */
		gathered = tess_gather_path(root, partial_rel, &partial->path);
		if (gathered != NULL)
		{
			final = create_agg_path(root, grouped_rel, gathered, grouped_rel->reltarget,
									strategy, AGGSPLIT_FINAL_DESERIAL,
									keys != NIL ? root->processed_groupClause : NIL,
									(List *) extra->havingQual,
									&extra->agg_final_costs, keys != NIL ? groups : 1.0);
			path = make_agg_path(root, final, final_tlist, list_length(keys),
								 AGG_PATH_HAVING | AGG_PATH_FINALIZE |
								 (nown > 0 ? AGG_PATH_OWN_STATES : 0), NULL);
			if (path != NULL)
			{
				add_path(grouped_rel, &path->path);
				continue;
			}
		}
		/* The core's Finalize reads no sum state of the node's format. */
		if (nsums > 0)
			continue;
		if (nown > 0)
		{
			partial = make_agg_path(root, agg, tlist, 0, AGG_PATH_PARTIAL, NULL);
			if (partial == NULL)
				continue;
			partial->path.parallel_aware = true;
		}
		rows = compute_gather_rows(&partial->path);
		gather = create_gather_path(root, partial_rel, &partial->path,
									partial->path.pathtarget, NULL, &rows);
		final = create_agg_path(root, grouped_rel, &gather->path,
								grouped_rel->reltarget, strategy,
								AGGSPLIT_FINAL_DESERIAL,
								keys != NIL ? root->processed_groupClause : NIL,
								(List *) extra->havingQual,
								&extra->agg_final_costs,
								keys != NIL ? groups : 1.0);
		add_path(grouped_rel, &final->path);
	}
}

/* Whether an aggregate of the target list has DISTINCT. */
static bool
has_distinct_aggregate(List *tlist)
{
	foreach_node(TargetEntry, entry, tlist)
		if (IsA(entry->expr, Aggref) && distinct_matters((Aggref *) entry->expr))
			return true;
	return false;
}

/*
 * The bytes the pairs of group and argument of the DISTINCT aggregates
 * take together, with their arguments' dictionaries, as the planner
 * estimates them, and into *words whether a word holds every argument. A
 * pair is the reference of its group's record and the argument, or the
 * argument alone without GROUP BY.
 */
static double
distinct_bytes(PlannerInfo *root, RelOptInfo *input_rel, List *keys, List *tlist,
			   bool *words, PairsEstimate *each)
{
	double		bytes = 0;

	*words = true;
	each->sets = 0;
	each->set_pairs = palloc_array(double, Max(list_length(tlist), 1));

	foreach_node(TargetEntry, entry, tlist)
	{
		Aggref	   *agg;
		double		pairs;

		if (!IsA(entry->expr, Aggref) || !distinct_matters((Aggref *) entry->expr))
			continue;
		agg = (Aggref *) entry->expr;
		pairs = estimate_num_groups(root,
									lappend(list_copy(keys), aggregate_argument(agg)),
									input_rel->rows, NULL, NULL);
		pairs *= (double) tess_table_record_bytes(keys != NIL ? 2 : 1, 0);
		bytes += pairs;
		each->set_pairs[each->sets++] = pairs;
		/* A dictionary of the values a word does not hold: an entry and a copy each. */
		{
			Node	   *argument = aggregate_argument(agg);
			TessTableKeyKind kind;

			if (!tess_word_key_kind(exprType(argument), &kind))
			{
				*words = false;
				bytes += estimate_num_groups(root, list_make1(argument), input_rel->rows,
											 NULL, NULL) *
					(sizeof(KeyEntry) * 2 + sizeof(Datum) +
					 get_typavgwidth(exprType(argument), exprTypmod(argument)));
			}
		}
	}
	return bytes;
}

/*
 * SELECT DISTINCT is grouping without aggregates: the node's path next to
 * each of the core's hashed distinct paths, over the same input, its keys
 * the distinct expressions, and the parallel stack over each of its
 * partial ones. DISTINCT ON, which keeps other columns of a row of each
 * group, needs the order and stays with the core.
 */
static void
create_distinct_paths(PlannerInfo *root, RelOptInfo *input_rel,
					  RelOptInfo *output_rel)
{
	List	   *keys;
	List	   *tlist;
	double		groups = 0;

	if (root->parse->hasDistinctOn || IS_DUMMY_REL(input_rel))
		return;
	keys = clause_keys(root, root->processed_distinctClause);
	if (keys == NIL)
		return;
	tlist = add_to_flat_tlist(NIL, keys);
	/* add_path changes the list: the candidates are taken first. */
	foreach_ptr(AggPath, agg, aggregate_templates(output_rel->pathlist,
												  AGG_HASHED, AGGSPLIT_SIMPLE))
	{
		CustomPath *path;

		if (agg->groupClause == NIL ||
			not_from_groups((Node *) agg->path.pathtarget->exprs, keys))
			continue;
		path = make_agg_path(root, agg, tlist, list_length(keys), 0, NULL);
		if (path != NULL)
			add_path(output_rel, &path->path);
	}
	foreach_ptr(Path, path, output_rel->pathlist)
		groups = Max(groups, path->rows);
	create_key_stack_paths(root, partial_upper_rel(root, UPPERREL_PARTIAL_DISTINCT, output_rel),
						   output_rel, AGGSPLIT_SIMPLE, keys, NULL,
						   root->processed_distinctClause, 0, groups);
}

/* The relations of a set operation's leaves, the tree's whole in the top one. */
static Relids
setop_leaves(Node *node, Relids leaves)
{
	if (IsA(node, RangeTblRef))
		return bms_add_member(leaves, ((RangeTblRef *) node)->rtindex);
	leaves = setop_leaves(((SetOperationStmt *) node)->larg, leaves);
	return setop_leaves(((SetOperationStmt *) node)->rarg, leaves);
}

/*
 * The set operation of the tree whose leaves are relids, the uppermost of
 * those the core folds into one (UNION of UNIONs); NULL for none.
 */
static SetOperationStmt *
setop_of(Node *node, Relids relids)
{
	SetOperationStmt *op;
	SetOperationStmt *found;

	if (!IsA(node, SetOperationStmt))
		return NULL;
	op = (SetOperationStmt *) node;
	if (bms_equal(setop_leaves(node, NULL), relids))
		return op;
	found = setop_of(op->larg, relids);
	return found != NULL ? found : setop_of(op->rarg, relids);
}

/*
 * Whether the core projected the paths of a set operation within another to
 * the other's column types: it does so before the hook, so a path added
 * here would lack the projection, and one over it could not find the set
 * operation's columns in the node's plan, which shows its first branch's.
 */
static bool
setop_projected(RelOptInfo *rel)
{
	foreach_ptr(Path, path, rel->pathlist)
		if (!equal(path->pathtarget->exprs, rel->reltarget->exprs))
			return true;
	return false;
}

/*
 * The partial Append of a UNION's branches, as the core builds it for its
 * Gather (generate_union_paths), which keeps it nowhere else: the first
 * partial path of each branch of append; NULL when a branch has none or
 * may not run in parallel.
 */
static Path *
union_partial_append(PlannerInfo *root, RelOptInfo *rel, AppendPath *append)
{
#if PG_VERSION_NUM >= 190000
	AppendPathInput input = {0};
#else
	/* PostgreSQL 18's create_append_path takes the lists one by one. */
	struct
	{
		List	   *partial_subpaths;
	}			input = {0};
#endif
	int			workers = 0;

	if (!rel->consider_parallel || max_parallel_workers_per_gather <= 0)
		return NULL;
	foreach_ptr(Path, subpath, append->subpaths)
	{
		RelOptInfo *branch = subpath->parent;
		Path	   *partial;

		if (!branch->consider_parallel || branch->partial_pathlist == NIL)
			return NULL;
		partial = linitial(branch->partial_pathlist);
		workers = Max(workers, partial->parallel_workers);
		input.partial_subpaths = lappend(input.partial_subpaths, partial);
	}
	if (enable_parallel_append)
	{
		workers = Max(workers, pg_leftmost_one_pos32(list_length(input.partial_subpaths)) + 1);
		workers = Min(workers, max_parallel_workers_per_gather);
	}
	if (workers <= 0)
		return NULL;
#if PG_VERSION_NUM >= 190000
	return (Path *) create_append_path(root, rel, input, NIL, NULL, workers,
									   enable_parallel_append, -1);
#else
	return (Path *) create_append_path(root, rel, NIL, input.partial_subpaths, NIL,
									   NULL, workers, enable_parallel_append, -1);
#endif
}

/*
 * UNION without ALL is grouping of the branches' rows by every column: the
 * node's path next to each of the core's hashed aggregate paths over the
 * Append of the branches, the node's Append below it. Above a set
 * operation the core puts a sort and a limit, or the Append, SetOp or
 * aggregate of another, which read its columns by position, where the
 * node's plan shows its first branch's targets in place of the set
 * operation's columns (tess_plan_setop_columns); one the core projected to
 * another's column types stays the core's (setop_projected), and so do the
 * operations of a recursive union, whose worktable rescans them. Once, the
 * parallel stack over the branches' partial Append: the node's partial
 * grouping over its parallel Append in every participant, TessGather, and
 * its grouping of their groups above.
 */
static void
create_setop_paths(PlannerInfo *root, RelOptInfo *output_rel)
{
	SetOperationStmt *top;
	List	   *keys;
	List	   *tlist;
	bool		stacked = false;

	if (root->parse->setOperations == NULL || root->hasRecursion ||
		IS_DUMMY_REL(output_rel))
		return;
	top = setop_of(root->parse->setOperations, output_rel->relids);
	if (top == NULL || setop_projected(output_rel))
		return;
	if (top->op != SETOP_UNION)
	{
		create_nonunion_paths(root, output_rel);
		return;
	}
	/* add_path changes the list: the candidates are taken first. */
	foreach_ptr(AggPath, agg, aggregate_templates(output_rel->pathlist,
												  AGG_HASHED, AGGSPLIT_SIMPLE))
	{
		CustomPath *path;
		Path	   *branches = agg->subpath;

		/* The partial Append under the core's Gather. */
		if (IsA(branches, GatherPath))
			branches = ((GatherPath *) branches)->subpath;
		keys = agg->path.pathtarget->exprs;
		if (!IsA(branches, AppendPath) || keys == NIL ||
			tess_path_setop_constant(branches) ||
			list_length(keys) > TESS_TABLE_MAX_KEYS ||
			list_length(agg->groupClause) != list_length(keys))
			continue;
		foreach_ptr(Node, key, keys)
		{
			TessTableKeyKind kind;
			SortGroupClause *clause = list_nth_node(SortGroupClause, agg->groupClause,
													foreach_current_index(key));

			if (!tess_word_key_kind(exprType(key), &kind) && !clause->hashable)
			{
				keys = NIL;
				break;
			}
		}
		if (keys == NIL)
			continue;
		if (!stacked)
		{
			/* Under the core's Gather, or as the core would build it. */
			Path	   *partial_append = branches != agg->subpath ? branches :
				union_partial_append(root, output_rel, (AppendPath *) branches);

			stacked = true;
			if (partial_append != NULL)
			{
				/* A participant's groups: no more than its rows. */
				AggPath    *partial = create_agg_path(root, output_rel, partial_append,
													  agg->path.pathtarget, AGG_HASHED,
													  AGGSPLIT_SIMPLE, agg->groupClause,
													  NIL, NULL,
													  Min(agg->path.rows,
														  partial_append->rows));

				add_key_stack_path(root, output_rel, output_rel, partial, keys, NULL,
								   agg->groupClause, 0, agg->path.rows);
			}
		}
		if (branches != agg->subpath)
			continue;
		tlist = add_to_flat_tlist(NIL, keys);
		path = make_agg_path(root, agg, tlist, list_length(keys), 0, NULL);
		if (path != NULL)
			add_path(output_rel, &path->path);
	}
}

/*
 * A key's equality for the private data: 0 for a word key, else its type's
 * default equality, which one of the clauses must use; false when the type
 * has none that hashes.
 */
static bool
key_eqop(Node *key, List *clauses, int *eqop)
{
	TessTableKeyKind kind;
	TypeCacheEntry *type;
	bool		used = false;

	*eqop = 0;
	if (tess_word_key_kind(exprType(key), &kind))
		return true;
	type = lookup_type_cache(exprType(key), TYPECACHE_EQ_OPR | TYPECACHE_HASH_PROC);
	foreach_node(SortGroupClause, clause, clauses)
		used |= clause->eqop == type->eq_opr && clause->hashable;
	if (!OidIsValid(type->eq_opr) || !OidIsValid(type->hash_proc) || !used)
		return false;
	*eqop = (int) type->eq_opr;
	return true;
}

/*
 * INTERSECT and EXCEPT, with ALL or not, are grouping of both sides' rows
 * by every column, the left side's first, counting each group's rows and
 * its right side's; each group then goes out as many times as the
 * operation says. The node's path next to each of the core's SetOp paths
 * of a set operation the node takes (as for UNION), its two batch children
 * the sides' paths below any sort; the groups spill past hash_mem, where
 * the core's hashed SetOp would not be chosen. The private data is the
 * grouping one with the command in its place.
 */

static void
create_nonunion_paths(PlannerInfo *root, RelOptInfo *output_rel)
{
	foreach_ptr(Path, candidate, list_copy(output_rel->pathlist))
	{
		SetOpPath  *setop;
		TessPathConfig config = TESS_STRUCT_INITIALIZER(TessPathConfig);
		List	   *keys;
		List	   *data;
		Path	   *left;
		Path	   *right;
		Path		template;
		Cost		own;
		bool		dictionary = false;

		if (!IsA(candidate, SetOpPath))
			continue;
		setop = (SetOpPath *) candidate;
		keys = setop->path.pathtarget->exprs;
		if (keys == NIL || tess_path_setop_constant(&setop->path) ||
			list_length(keys) > TESS_TABLE_MAX_KEYS ||
			list_length(setop->groupList) != list_length(keys))
			continue;
		data = list_make4_int(list_length(keys),
							  (int) Min(setop->numGroups, (double) PG_INT32_MAX),
							  (int) setop->cmd, 0);
		foreach_ptr(Node, key, keys)
		{
			int			eqop;

			if (!key_eqop(key, setop->groupList, &eqop))
			{
				data = NIL;
				break;
			}
			dictionary |= eqop != 0;
			data = lappend_int(data, eqop);
		}
		if (data == NIL)
			continue;
		data = lappend_int(data, 0);
		left = setop->leftpath;
		right = setop->rightpath;
		/* Sorts the core put below for its sorted SetOp: hashing needs none. */
		while (IsA(left, SortPath) || IsA(left, IncrementalSortPath))
			left = ((SortPath *) left)->subpath;
		while (IsA(right, SortPath) || IsA(right, IncrementalSortPath))
			right = ((SortPath *) right)->subpath;
		left = tess_batch_input_path(root, left);
		right = left == NULL ? NULL : tess_batch_input_path(root, right);
		if (right == NULL)
			continue;
		/*
		 * The sides' batch paths and a share of what the core's SetOp costs
		 * over its own, tessera.setop_word_share (0.5) and
		 * tessera.setop_dictionary_share (0.9): the node's took 0.55 of the
		 * core's time with keys of words, 0.9 with a key through a dictionary
		 * (plan 5.13, step 5).
		 * All of it before the first row, as the groups are made first.
		 */
		own = setop->path.total_cost - setop->leftpath->total_cost -
			setop->rightpath->total_cost;
		template = setop->path;
		template.total_cost = left->total_cost + right->total_cost +
			Max(own, 0) * (dictionary ? tess_setop_dictionary_share : tess_setop_word_share);
		template.startup_cost = template.total_cost;
		template.pathkeys = NIL;
		config.template_path = &template;
		config.methods = &agg_path_methods;
		config.node = &tess_agg_node;
		config.children = list_make2(left, right);
		config.expressions = add_to_flat_tlist(NIL, keys);
		config.node_data = (Node *) data;
		add_path(output_rel, &tess_path_create(&config)->path);
	}
}

/*
 * The node's path in place of each of the core's plain aggregate paths
 * whose input can be read in batches, and the parallel stack in place of
 * each partial one.
 */
static void
create_upper_paths(PlannerInfo *root, UpperRelationKind stage,
				   RelOptInfo *input_rel, RelOptInfo *output_rel, void *extra)
{
	List	   *keys = NIL;
	List	   *tlist = NIL;
	List	   *templates;
	AggStrategy strategy = AGG_PLAIN;
	double		groups = 0;
	PairsEstimate spill;
	const PairsEstimate *pairs = NULL;
	AggFold		fold;
	List	   *states;

	if (previous_create_upper_paths_hook != NULL)
		previous_create_upper_paths_hook(root, stage, input_rel, output_rel,
										 extra);
	if (!tess_enabled())
		return;
	if (stage == UPPERREL_DISTINCT)
	{
		create_distinct_paths(root, input_rel, output_rel);
		return;
	}
	if (stage == UPPERREL_SETOP)
	{
		create_setop_paths(root, output_rel);
		return;
	}
	if (stage != UPPERREL_GROUP_AGG ||
		!query_supported(root, input_rel, output_rel))
		return;
	if (root->parse->groupClause != NIL)
	{
		/* The grouping expressions lead the scan tuple, the table's keys. */
		keys = group_keys(root);
		if (keys == NIL)
			return;
		tlist = add_to_flat_tlist(NIL, keys);
		strategy = AGG_HASHED;
	}
	if (!collect_aggregates((Node *) list_make2(output_rel->reltarget->exprs,
												root->parse->havingQual),
							keys, &tlist) ||
		tlist == NIL)
		return;
	/*
	 * The node keeps AGG_MAX_GROUPED states at most, with groups or without,
	 * those of the aggregates it folds counted once.
	 */
	fold_aggregates(root, tlist, list_length(keys), true, &fold);
	if (list_length(fold.states) > AGG_MAX_GROUPED)
		return;
	states = fold_target_list(tlist, list_length(keys), &fold);
	/*
	 * Beside a DISTINCT aggregate's table neither the groups nor the rows of
	 * generic aggregates spill: the groups with their states must fit. The
	 * pairs must fit too, or, with GROUP BY and a word for every argument,
	 * they go to disk, the groups leaving them half of hash_mem at least;
	 * only then does the node weigh the groups by the estimate.
	 */
	if (has_distinct_aggregate(states))
	{
		double		limit = (double) tess_hash_memory_limit();
		bool		words;

		spill.pairs = distinct_bytes(root, input_rel, keys, states, &words, &spill);
		spill.groups = 0;
		if (keys != NIL)
		{
			spill.groups = groups_bytes(root, output_rel, list_length(keys), states);
			if (spill.groups < 0 || spill.groups > limit)
				return;
		}
		if (spill.pairs > limit)
		{
			if (keys == NIL || !words || spill.groups > limit / 2)
				return;
			pairs = &spill;
		}
	}
	/* add_path changes the list: the candidates are taken first. */
	templates = aggregate_templates(output_rel->pathlist, strategy, AGGSPLIT_SIMPLE);
	/*
	 * The core's sorted grouping may have beaten its hashed one out of the
	 * list, as its spill costs more (and with DISTINCT in an aggregate it
	 * groups only sorted): the node hashes in its place at its own cost,
	 * reading the input below the sort, unless hashing is disabled.
	 */
	if (templates == NIL && strategy == AGG_HASHED && enable_hashagg)
		templates = aggregate_templates(output_rel->pathlist, AGG_SORTED,
										AGGSPLIT_SIMPLE);
	foreach_ptr(AggPath, agg, templates)
	{
		CustomPath *path = make_agg_path(root, agg, tlist, list_length(keys),
										 AGG_PATH_HAVING, pairs);

		if (path != NULL)
			add_path(output_rel, &path->path);
	}
	/*
	 * The planner's estimate of the groups, for a Finalize HashAggregate:
	 * the rows of the core's grouped paths, the relation's own being set
	 * only after this hook.
	 */
	foreach_ptr(Path, path, output_rel->pathlist)
		groups = Max(groups, path->rows);
	create_partial_paths(root, output_rel, (GroupPathExtraData *) extra, keys, tlist,
						 groups);
}

/*
 * An argument's column as a Var of INDEX_VAR naming the child's target,
 * which the projection maps to its batch column through the child's
 * layout: the arguments cannot go through custom_exprs, which the planner
 * would fix against the scan tuple of aggregates, so they travel in the
 * private data with their targets resolved here.
 */
static Node *
resolve_argument(Node *node, TessPlanChild *child)
{
	TargetEntry *found;

	if (node == NULL)
		return NULL;
	/* A column of the child, or an expression it computes, as a grouping one. */
	found = IsA(node, Const) ? NULL : tess_plan_child_entry(child, (Expr *) node);
	if (found != NULL)
		return (Node *) makeVar(INDEX_VAR, found->resno, exprType(node),
								exprTypmod(node), exprCollation(node), 0);
	if (IsA(node, Var))
		elog(ERROR, "TessAgg argument is missing from its child");
	return expression_tree_mutator(node, resolve_argument, child);
}

/*
 * The parameters of the arguments, which the planner must see in
 * custom_exprs to count them among the plan's: a node above that
 * rescans its child only when its parameters changed would otherwise
 * keep a stale result.
 */
static bool
collect_params(Node *node, List **params)
{
	if (node == NULL)
		return false;
	if (IsA(node, Param))
	{
		*params = lappend(*params, node);
		return false;
	}
	return expression_tree_walker(node, collect_params, params);
}

/*
 * The scan tuple is the aggregates themselves, so that the planner turns
 * the targets and HAVING into references to it; the child's columns stay
 * hidden. HAVING is the plan's qual, as it is the core aggregate's, for the
 * query's grouping only (AGG_PATH_HAVING): not for the partial aggregates of
 * a parallel plan, whose Finalize Aggregate applies it, nor for a DISTINCT
 * or a set operation above. The private data carries the states the node
 * keeps, each an aggregate with one argument, a NULL constant for
 * count(*), and one FILTER condition, NULL without one; and for each
 * aggregate of the scan tuple its output, the state whose value it is or
 * a derivation from states (fold_aggregates), with the derivations'
 * states and constants.
 */
/*
 * A set operation's counts as aggregates of the scan tuple: count(*) of a
 * group's rows, and sum over the side, 0 for the left and 1 for the right,
 * which each side's projection computes as a constant of its own.
 */
static Aggref *
setop_count(bool side)
{
	Aggref	   *agg = makeNode(Aggref);

	agg->aggfnoid = side ? F_SUM_INT4 : F_COUNT_;
	agg->aggtype = INT8OID;
	agg->aggtranstype = INT8OID;
	agg->aggstar = !side;
	agg->aggkind = AGGKIND_NORMAL;
	agg->aggsplit = AGGSPLIT_SIMPLE;
	agg->aggno = -1;
	agg->aggtransno = -1;
	agg->location = -1;
	if (side)
	{
		agg->aggargtypes = list_make1_oid(INT4OID);
		agg->args = list_make1(makeTargetEntry((Expr *) makeConst(INT4OID, -1, InvalidOid,
																  sizeof(int32),
																  Int32GetDatum(0),
																  false, true),
											   1, NULL, false));
	}
	return agg;
}

/*
 * An argument of an aggregate with FILTER: an expression's value where the
 * condition holds, else NULL, so that a row the filter drops is never
 * computed, as the executor evaluates the arguments of the rows it keeps
 * alone; a chain computes a CASE branch over the rows it takes only. A
 * column or a constant, which no row makes fail, stays as it is, the
 * condition not computed again.
 */
static Node *
filtered_argument(Expr *condition, Node *argument)
{
	CaseExpr   *choice;
	CaseWhen   *when;

	if (IsA(argument, Var) || IsA(argument, Const))
		return argument;
	choice = makeNode(CaseExpr);
	when = makeNode(CaseWhen);

	when->expr = (Expr *) copyObject(condition);
	when->result = (Expr *) argument;
	when->location = -1;
	choice->casetype = exprType(argument);
	choice->casecollid = exprCollation(argument);
	choice->args = list_make1(when);
	choice->defresult = (Expr *) makeNullConst(exprType(argument), exprTypmod(argument),
											   exprCollation(argument));
	choice->location = -1;
	return (Node *) choice;
}

/*
 * A FILTER condition as a value the projection computes, true where it
 * holds: over the batch as `CASE WHEN condition THEN true END`, which the
 * expression compiler takes where it takes the condition, else the
 * condition itself, which the executor evaluates a row without the CASE.
 */
static Node *
filter_value(Expr *condition)
{
	CaseExpr   *choice = makeNode(CaseExpr);
	CaseWhen   *when = makeNode(CaseWhen);

	when->expr = condition;
	when->result = (Expr *) makeBoolConst(true, false);
	when->location = -1;
	choice->casetype = BOOLOID;
	choice->casecollid = InvalidOid;
	choice->args = list_make1(when);
	choice->defresult = (Expr *) makeNullConst(BOOLOID, -1, InvalidOid);
	choice->location = -1;
	return tess_expr_supports_value((Node *) choice, 0) ? (Node *) choice : (Node *) condition;
}

static Plan *
agg_plan(PlannerInfo *root, RelOptInfo *rel, CustomPath *best_path,
		 List *tlist, List *clauses, List *custom_plans)
{
	TessPathInfo info = TESS_STRUCT_INITIALIZER(TessPathInfo);
	TessPlanConfig config = TESS_STRUCT_INITIALIZER(TessPlanConfig);
	TessPlanChild child = TESS_STRUCT_INITIALIZER(TessPlanChild);
	List	   *arguments = NIL;
	List	   *more = NIL;
	List	   *filters = NIL;
	List	   *keys = NIL;
	List	   *params = NIL;
	AggFold		fold;
	List	   *path_data;
	TessPlanWriter *writer;
	int			nkeys;
	int			setop;
	int			flags;

	tess_path_get_info(best_path, &info);
	if (!tess_plan_child(best_path, custom_plans, 0, &child))
		elog(ERROR, "TessAgg expected a batch child");
	/* Over a set operation's rows: its columns are the child's targets. */
	tlist = (List *) tess_plan_setop_columns((Node *) tlist, child.plan);
	info.expressions = (List *) tess_plan_setop_columns((Node *) info.expressions, child.plan);
	path_data = (List *) info.node_data;
	nkeys = linitial_int(path_data);
	setop = lthird_int(path_data);
	flags = lfourth_int(path_data);
	/* INTERSECT or EXCEPT: the command, and two children. */
	if (setop >= 0)
	{
		TessPlanChild right = TESS_STRUCT_INITIALIZER(TessPlanChild);
		int			position = 0;

		if (!tess_plan_child(best_path, custom_plans, 1, &right))
			elog(ERROR, "TessAgg expected a second batch child");
		/*
		 * The keys are the sides' columns by position, the right side's the
		 * same types: a key found by its expression in the left side's
		 * targets could name another column of the right side.
		 */
		foreach_node(TargetEntry, entry, child.plan->targetlist)
		{
			TargetEntry *other;

			if (entry->resjunk || position == nkeys)
				continue;
			other = list_nth_node(TargetEntry, right.plan->targetlist, position);
			if (exprType((Node *) other->expr) != exprType((Node *) entry->expr))
				elog(ERROR, "TessAgg sides differ in the type of column %d", position + 1);
			keys = lappend(keys, makeVar(INDEX_VAR, entry->resno,
										 exprType((Node *) entry->expr),
										 exprTypmod((Node *) entry->expr),
										 exprCollation((Node *) entry->expr), 0));
			position++;
		}
		if (position != nkeys)
			elog(ERROR, "TessAgg side has %d columns, not %d", position, nkeys);
		info.expressions = lappend(info.expressions,
								   makeTargetEntry((Expr *) setop_count(false),
												   nkeys + 1, NULL, false));
		info.expressions = lappend(info.expressions,
								   makeTargetEntry((Expr *) setop_count(true),
												   nkeys + 2, NULL, false));
	}
	/* The grouping expressions, then the states of the aggregates. */
	foreach_ptr(TargetEntry, entry, info.expressions)
	{
		if (foreach_current_index(entry) < nkeys && setop < 0)
			keys = lappend(keys, resolve_argument((Node *) copyObject(entry->expr),
												  &child));
	}
	fold_aggregates(root, info.expressions, nkeys,
					setop < 0 && (flags & (AGG_PATH_PARTIAL | AGG_PATH_FINALIZE)) == 0, &fold);
	foreach_node(Aggref, agg, fold.states)
	{
		Node	   *argument;
		Expr	   *filter;

		/* Above a gather: the aggregate's partial value, a column of the child. */
		if (flags & AGG_PATH_FINALIZE)
		{
			Aggref	   *partial = copyObject(agg);

			mark_partial_aggref(partial, AGGSPLIT_INITIAL_SERIAL);
			arguments = lappend(arguments, resolve_argument((Node *) partial, &child));
			more = lappend(more, NIL);
			filters = lappend(filters, NULL);
			continue;
		}
		/* FILTER (WHERE ...): which rows the aggregate reads, NULL for every row. */
		filter = agg->aggfilter;
		filters = lappend(filters, filter == NULL ? NULL :
						  resolve_argument(filter_value(copyObject(filter)), &child));
		argument = aggregate_argument(agg);
		arguments = lappend(arguments, argument == NULL ?
							(Node *) makeNullConst(INT4OID, -1, InvalidOid) :
							resolve_argument(filter == NULL ? copyObject(argument) :
											 filtered_argument(filter, copyObject(argument)),
											 &child));
		/* The arguments after the first, of an aggregate the core's functions compute. */
		{
			List	   *rest = NIL;

			for (int n = 1; n < list_length(agg->args); n++)
			{
				Node	   *other = (Node *) copyObject(list_nth_node(TargetEntry, agg->args,
																	  n)->expr);

				rest = lappend(rest, resolve_argument(filter == NULL ? other :
													  filtered_argument(filter, other),
													  &child));
			}
			more = lappend(more, rest);
		}
	}
	collect_params((Node *) arguments, &params);
	collect_params((Node *) more, &params);
	collect_params((Node *) filters, &params);
	collect_params((Node *) keys, &params);
	writer = tess_plan_writer_create(TESS_AGG_DATA, TESS_AGG_DATA_VERSION);
	tess_plan_write_list(writer, "states", fold.states);
	tess_plan_write_int_list(writer, "outputs", fold.outputs);
	tess_plan_write_int_list(writer, "derived", fold.derived);
	tess_plan_write_list(writer, "constants", fold.constants);
	tess_plan_write_int_list(writer, "checks", fold.checks);
	tess_plan_write_list(writer, "probes", fold.probes);
	tess_plan_write_list(writer, "arguments", arguments);
	tess_plan_write_list(writer, "more", more);
	tess_plan_write_list(writer, "filters", filters);
	tess_plan_write_list(writer, "keys", keys);
	tess_plan_write_int(writer, "groups", lsecond_int(path_data));
	tess_plan_write_int_list(writer, "key_eqops",
							 list_copy_head(list_copy_tail(path_data, 4), nkeys));
	tess_plan_write_int(writer, "setop", setop);
	tess_plan_write_int(writer, "partial", (flags & AGG_PATH_PARTIAL) != 0);
	tess_plan_write_int(writer, "own_states", (flags & AGG_PATH_OWN_STATES) != 0);
	tess_plan_write_int(writer, "finalize", (flags & AGG_PATH_FINALIZE) != 0);
	tess_plan_write_int(writer, "groups_kb", list_nth_int(path_data, 4 + nkeys));
	config.methods = &tess_agg_scan_methods;
	config.layout_policy = TESS_LAYOUT_DENSE;
	config.qual = (flags & AGG_PATH_HAVING) ? (List *) root->parse->havingQual : NIL;
	config.expressions = params;
	config.scan_targetlist = info.expressions;
	config.scanrelid = 0;
	config.node_data = (Node *) tess_plan_writer_finish(writer);
	return tess_plan_create(best_path, tlist, custom_plans, &config);
}

void
tess_agg_planner_init(void)
{
	previous_create_upper_paths_hook = create_upper_paths_hook;
	create_upper_paths_hook = create_upper_paths;
}

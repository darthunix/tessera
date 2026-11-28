#include "postgres.h"

#include "access/htup_details.h"
#include "access/nbtree.h"
#include "catalog/pg_aggregate.h"
#include "commands/explain_format.h"
#include "common/hashfn.h"
#include "common/int.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "parser/parse_agg.h"
#include "utils/array.h"
#include "utils/float.h"
#include "utils/fmgroids.h"
#include "utils/syscache.h"
#include "utils/typcache.h"
#include "utils/lsyscache.h"
#include "utils/datum.h"
#include "utils/builtins.h"
#include "utils/numeric.h"
#include "utils/pg_locale.h"
#include "utils/regproc.h"
#include "utils/ruleutils.h"

#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"
#include "agg.h"
#include "agg_node.h"

/*
 * TessAgg computes the aggregates of a query over the batches of a batch
 * child, so that no row is handed up one at a time. Without GROUP BY it
 * stands in for the core's plain Aggregate and returns the one result
 * row: each aggregate is computed per batch by the registered batch
 * function of its aggregate (tessera/function.h, kind
 * TESS_FUNCTION_AGGREGATE) and the partials are combined here, with the
 * overflow check the core's transition would make. With GROUP BY it
 * stands in for the core's HashAggregate: each row finds the record of
 * its keys in a hash table (tessera/table.h), whose payload holds the
 * group's aggregate states, and the groups go out in batches when the
 * input ends. See docs/nodes.md.
 */
/* A batch with at most this many survivors is gathered for one call later. */
#define AGG_GATHER_ROWS 8

/* The counters every participant of a parallel plan shares. */
enum
{
	AGG_BATCHES,
	AGG_ROWS,
	AGG_CALLS,
	AGG_COMPUTED,
	AGG_GROUPS,
	AGG_MEMORY,
	AGG_GROWS,
	/*
	 * Spilling: the most partitions of a level, the partitions sent to
	 * disk while the input was read, the chunks and bytes written, the
	 * partitions split into a level below.
	 */
	AGG_PARTITIONS,
	AGG_EVICTIONS,
	AGG_SPILLED,
	AGG_DISK,
	AGG_SPLITS,
	/* Partial mode: the times the groups went out before the input ended. */
	AGG_EARLY,
	/* Generic states past hash_mem: the rows sent to partitions on disk. */
	AGG_SPILLED_ROWS,
	/* DISTINCT sets past hash_mem: the pairs sent to partitions on disk. */
	AGG_SPILLED_PAIRS,
	/* The input rows copied into gathered batches. */
	AGG_GATHERED,
	AGG_NCOUNTERS
};

static const CustomExecMethods agg_exec_methods;
static const TessBatchOps groups_batch_ops;

/*
 * Whether a state is the node's own sum state of avg of an integer, of
 * bigint or of DISTINCT values, which counts them too, or of numeric.
 */
static bool
integer_sum_state(const AggValue *value)
{
	return value->kind == AGG_GENERIC && value->generic->fast == FAST_AVG &&
		!value->generic->fast_numeric && !OidIsValid(value->generic->fast_float);
}

static bool
numeric_sum_state(const AggValue *value)
{
	return value->kind == AGG_GENERIC && value->generic->fast == FAST_AVG &&
		value->generic->fast_numeric;
}

/* Whether a state is the node's own extreme of numeric. */
static bool
extreme_state(const AggValue *value)
{
	return value->kind == AGG_GENERIC && value->generic->fast_numeric &&
		(value->generic->fast == FAST_MIN || value->generic->fast == FAST_MAX);
}

/*
 * Whether a derived output's states are those it derives from: a count
 * alone, count(DISTINCT e) or count(*) of a constant's rows; an extreme
 * of e alone; the words of sum(e), or the sum state of avg(e) of numeric,
 * with count(e); or the sum state of avg(e) of an integer alone.
 */
static bool
derived_sources(const TessAggState *state, const AggOutput *output)
{
	const AggValue *source = &state->values[output->source];
	const AggValue *count = output->count_state >= 0 ?
		&state->values[output->count_state] : NULL;

	/* count(DISTINCT e) of a numeric, or count(*) of the constants' rows. */
	if (count == source)
		return source->kind == AGG_COUNT;
	if (output->kind == AGG_DERIVED_MIN || output->kind == AGG_DERIVED_MAX)
		return count == NULL &&
			(source->kind == AGG_MIN || source->kind == AGG_MAX || extreme_state(source));
	if (count == NULL)
		return integer_sum_state(source);
	return (source->kind == AGG_SUM || numeric_sum_state(source)) && count->kind == AGG_COUNT;
}

#ifdef HAVE_INT128
/*
 * A derivation's constant as a decimal, a mantissa at a scale: an int8 at
 * scale 0, a numeric by its digits as numeric_out writes them, its scale
 * the count after the point; false past 10^36.
 */
static bool
constant_decimal(const Const *constant, int128 *mantissa, int *scale)
{
	const char *text;
	bool		negative;

	*mantissa = 0;
	*scale = 0;
	if (constant->consttype == INT8OID)
	{
		*mantissa = DatumGetInt64(constant->constvalue);
		return true;
	}
	text = DatumGetCString(DirectFunctionCall1(numeric_out, constant->constvalue));
	negative = *text == '-';
	for (const char *digit = text + negative; *digit != '\0'; digit++)
	{
		if (*digit == '.')
		{
			*scale = (int) strlen(digit + 1);
			continue;
		}
		/* 10^35 before the next digit: 10^36 at most after it. */
		if (*digit < '0' || *digit > '9' ||
			*mantissa > (int128) INT64CONST(100000000000000000) * INT64CONST(1000000000000000000))
			return false;
		*mantissa = *mantissa * 10 + (*digit - '0');
	}
	if (negative)
		*mantissa = -*mantissa;
	return true;
}
#endif

/*
 * The checks of errors of the plan: each of a state whose argument is e,
 * which takes e's extremes from its column, and the probes, the forms'
 * arguments over e as the slot's one column, which agg_check_finish
 * evaluates.
 */
static void
init_checks(TessAggState *state, EState *estate, List *arguments, List *checks,
			List *probes)
{
	if (list_length(checks) != list_length(probes) * AGG_CHECK_FIELDS)
		elog(ERROR, "TessAgg received a foreign plan");
	state->nchecks = list_length(probes);
	state->checks = palloc0_array(AggCheck, Max(state->nchecks, 1));
	foreach_node(List, forms, probes)
	{
		int			number = foreach_current_index(forms);
		AggCheck   *check = &state->checks[number];
		int			index = list_nth_int(checks, number * AGG_CHECK_FIELDS);
		Oid			type = (Oid) list_nth_int(checks, number * AGG_CHECK_FIELDS + 1);
		AggValue   *value;
		Oid			column;

		if (index < 0 || index >= state->nvalues || forms == NIL ||
			(type != INT2OID && type != INT4OID && type != INT8OID))
			elog(ERROR, "TessAgg received a foreign plan");
		value = &state->values[index];
		column = exprType(list_nth(arguments, index));
		if (value->check >= 0 || value->computed < 0 ||
			(column != INT4OID && column != INT8OID))
			elog(ERROR, "TessAgg received a foreign plan");
		value->check = number;
		check->wide = column == INT8OID;
		check->type = type;
		check->slot = ExecInitExtraTupleSlot(estate,
											 ExecTypeFromTL(list_make1(makeTargetEntry((Expr *) makeNullConst(type, -1, InvalidOid),
																					   1, NULL, false))),
											 &TTSOpsVirtual);
		foreach_ptr(Expr, probe, forms)
			check->probes = lappend(check->probes,
									ExecInitExpr(probe, &state->css.ss.ps));
	}
}

/* A derivation's constant's scale: 0 of an int8, a numeric's own. */
static int
constant_scale(const Const *constant)
{
	if (constant->consttype != NUMERICOID)
		return 0;
	return DatumGetInt32(DirectFunctionCall1(numeric_scale, constant->constvalue));
}

/* A derivation's constant as a numeric: an int8 one, or a numeric past it. */
static Datum
constant_numeric(const Const *constant)
{
	if (constant->constisnull)
		elog(ERROR, "TessAgg received a foreign plan");
	if (constant->consttype == INT8OID)
		return NumericGetDatum(int64_to_numeric(DatumGetInt64(constant->constvalue)));
	if (constant->consttype != NUMERICOID)
		elog(ERROR, "TessAgg received a foreign plan");
	return constant->constvalue;
}

/*
 * The outputs of the plan, after its states: each aggregate of the scan
 * tuple the value of a state, or derived from two, the sum of its e and
 * their count, with its constants a and b, by its aggregate's kind.
 */
static void
derived_outputs(TessAggState *state, CustomScan *cscan, List *states, List *outputs,
				List *derived, List *constants)
{
	state->outputs = palloc0_array(AggOutput, Max(state->noutputs, 1));
	foreach_int(index, outputs)
	{
		int			position = foreach_current_index(index);
		AggOutput  *output = &state->outputs[position];
		Aggref	   *agg = (Aggref *) list_nth_node(TargetEntry, cscan->custom_scan_tlist,
												   state->nkeys + position)->expr;
		int			number = -1 - index;
		const Const *a;
		const Const *b;

		output->state = index;
		if (!IsA(agg, Aggref) || index >= state->nvalues ||
			(index < 0 && number >= list_length(derived) / AGG_DERIVED_STATES))
			elog(ERROR, "TessAgg received a foreign plan");
		/* A state's own value: of the aggregate in the scan tuple's place. */
		if (index >= 0)
		{
			Aggref	   *own = list_nth(states, index);

			if (own->aggfnoid != agg->aggfnoid || own->aggtype != agg->aggtype)
				elog(ERROR, "TessAgg received a foreign plan");
			continue;
		}
		output->source = list_nth_int(derived, number * AGG_DERIVED_STATES);
		output->count_state = list_nth_int(derived, number * AGG_DERIVED_STATES + 1);
		if (output->source < 0 || output->source >= state->nvalues ||
			output->count_state < -1 || output->count_state >= state->nvalues)
			elog(ERROR, "TessAgg received a foreign plan");
		switch (agg->aggfnoid)
		{
			case F_MIN_INT2:
			case F_MIN_INT4:
			case F_MIN_INT8:
				output->kind = AGG_DERIVED_MIN;
				output->type = agg->aggtype;
				break;
			case F_MAX_INT2:
			case F_MAX_INT4:
			case F_MAX_INT8:
				output->kind = AGG_DERIVED_MAX;
				output->type = agg->aggtype;
				break;
			case F_SUM_INT2:
			case F_SUM_INT4:
				output->kind = AGG_DERIVED_SUM_INT8;
				break;
			case F_SUM_INT8:
				output->kind = AGG_DERIVED_SUM_NUMERIC;
				break;
			case F_AVG_INT2:
			case F_AVG_INT4:
				output->kind = AGG_DERIVED_AVG_INT8;
				break;
			case F_AVG_INT8:
			case F_AVG_NUMERIC:
				output->kind = AGG_DERIVED_AVG_NUMERIC;
				break;
			case F_SUM_NUMERIC:
				output->kind = AGG_DERIVED_SUM_NUMERIC;
				break;
			case F_MIN_NUMERIC:
				output->kind = AGG_DERIVED_MIN;
				output->type = NUMERICOID;
				break;
			case F_MAX_NUMERIC:
				output->kind = AGG_DERIVED_MAX;
				output->type = NUMERICOID;
				break;
			case F_COUNT_ANY:
				output->kind = AGG_DERIVED_COUNT;
				break;
			default:
				elog(ERROR, "TessAgg received a foreign plan");
		}
		if (!derived_sources(state, output))
			elog(ERROR, "TessAgg received a foreign plan");
		a = list_nth(constants, number * AGG_DERIVED_CONSTANTS);
		b = list_nth(constants, number * AGG_DERIVED_CONSTANTS + 1);
		if (!IsA(a, Const) || !IsA(b, Const))
			elog(ERROR, "TessAgg received a foreign plan");
		output->a_numeric = constant_numeric(a);
		output->b_numeric = constant_numeric(b);
		/* A form whose value is an integer has integer constants. */
		if ((output->kind == AGG_DERIVED_SUM_INT8 || output->kind == AGG_DERIVED_AVG_INT8 ||
			 ((output->kind == AGG_DERIVED_MIN || output->kind == AGG_DERIVED_MAX) &&
			  output->type != NUMERICOID)) &&
			(constant_scale(a) != 0 || constant_scale(b) != 0))
			elog(ERROR, "TessAgg received a foreign plan");
#ifdef HAVE_INT128
		output->exact = constant_decimal(a, &output->a, &output->a_scale) &&
			constant_decimal(b, &output->b, &output->b_scale);
#else
		/* The planner folds nothing without 128-bit integers. */
		elog(ERROR, "TessAgg received a foreign plan");
#endif
		state->nderived++;
		/*
		 * By reference: a numeric, or the numerics the derivation makes on
		 * its way to an integer where a and b pass an int8 and 128 bits may
		 * not hold their products; in memory a group resets.
		 */
		state->derived_by_ref |= output->kind == AGG_DERIVED_SUM_NUMERIC ||
			output->kind == AGG_DERIVED_AVG_INT8 || output->kind == AGG_DERIVED_AVG_NUMERIC ||
			output->type == NUMERICOID ||
			(output->kind != AGG_DERIVED_COUNT &&
			 (a->consttype != INT8OID || b->consttype != INT8OID));
	}
}

#ifdef HAVE_INT128
/* A decimal at a scale raised to a greater one, false past 128 bits. */
static bool
decimal_rescale(int128 value, int from, int to, int128 *result)
{
	*result = value;
	for (int scale = from; scale < to; scale++)
		if (__builtin_mul_overflow(*result, 10, result))
			return false;
	return true;
}

/*
 * a * sum + b * count of a derived output as a decimal in 128 bits, at
 * the greater of the scales of the two products, as numeric_mul and
 * numeric_add give it; false where a step passes them, the sum has a
 * rest or NaN, or a constant is past 10^36.
 */
static bool
derived_exact(const AggOutput *output, const AggSum *sum, Datum *total, int64 *narrow,
			  bool bigint)
{
	int			scale = Max(output->a_scale + sum->scale, output->b_scale);
	int128		left;
	int128		right;
	int128		exact;

	if (!output->exact || sum->rest != (Datum) 0 ||
		__builtin_mul_overflow(output->a, sum->sum, &left) ||
		!decimal_rescale(left, output->a_scale + sum->scale, scale, &left) ||
		__builtin_mul_overflow(output->b, (int128) sum->count, &right) ||
		!decimal_rescale(right, output->b_scale, scale, &right) ||
		__builtin_add_overflow(left, right, &exact))
		return false;
	if (!bigint)
		*total = agg_int128_numeric(exact, scale);
	else if (exact < PG_INT64_MIN || exact > PG_INT64_MAX)
		ereport(ERROR,
				(errcode(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
				 errmsg("bigint out of range")));
	else
		*narrow = (int64) exact;
	return true;
}

/*
 * A derived output's value from the sum of its e and their count, as the
 * core's aggregate of its form gives it: a * sum + b * count, exact in 128
 * bits and in numeric where a step passes them, NaN after a NaN; a bigint
 * for a form of smallint or integer, 22003 past one, as numeric_int8
 * refuses it; the average numeric_div of that sum and the count, as the
 * core's final functions divide; NULL without a row, but 0 for count.
 */
Datum
agg_derive(const AggOutput *output, const AggSum *sum, bool *isnull)
{
	bool		bigint = output->kind == AGG_DERIVED_SUM_INT8 ||
		output->kind == AGG_DERIVED_AVG_INT8;
	Datum		total = (Datum) 0;
	int64		narrow = 0;

	*isnull = false;
	if (output->kind == AGG_DERIVED_COUNT)
		return Int64GetDatum(sum->count);
	if (sum->count == 0)
	{
		*isnull = true;
		return (Datum) 0;
	}
	if (!derived_exact(output, sum, &total, &narrow, bigint))
	{
		Datum		whole = agg_int128_numeric(sum->sum, sum->scale);

		if (sum->rest != (Datum) 0)
			whole = DirectFunctionCall2(numeric_add, sum->rest, whole);
		total = DirectFunctionCall2(numeric_add,
									DirectFunctionCall2(numeric_mul, output->a_numeric, whole),
									DirectFunctionCall2(numeric_mul, output->b_numeric,
														NumericGetDatum(int64_to_numeric(sum->count))));
		if (bigint)
			narrow = DatumGetInt64(DirectFunctionCall1(numeric_int8, total));
	}
	if (output->kind == AGG_DERIVED_SUM_INT8)
		return Int64GetDatum(narrow);
	if (bigint)
		total = NumericGetDatum(int64_to_numeric(narrow));
	if (output->kind == AGG_DERIVED_SUM_NUMERIC)
		return total;
	return DirectFunctionCall2(numeric_div, total,
							   NumericGetDatum(int64_to_numeric(sum->count)));
}

/*
 * min or max of a form, from the extreme of e that gives it, the least
 * where the form grows with e: a * e + b, which the check of errors keeps
 * within each step's type (a form that cannot fail needs none), in the
 * form's type; of a numeric form a numeric, NaN of a NaN; NULL without a
 * row. The extreme is a numeric where e is one, else an integer.
 */
Datum
agg_derive_extreme(const AggOutput *output, Datum extreme, bool numeric, int64 e,
				   bool *isnull)
{
	int128		value;

	if (*isnull)
		return (Datum) 0;
	if (output->type == NUMERICOID)
		return DirectFunctionCall2(numeric_add,
								   DirectFunctionCall2(numeric_mul, output->a_numeric,
													   numeric ? extreme :
													   NumericGetDatum(int64_to_numeric(e))),
								   output->b_numeric);
	/* In 128 bits where they hold it, else in numeric. */
	if (!output->exact || output->a_scale != 0 || output->b_scale != 0 ||
		__builtin_mul_overflow(output->a, (int128) e, &value) ||
		__builtin_add_overflow(value, output->b, &value))
		value = DatumGetInt64(DirectFunctionCall1(numeric_int8,
												  DirectFunctionCall2(numeric_add,
																	  DirectFunctionCall2(numeric_mul,
																						  output->a_numeric,
																						  NumericGetDatum(int64_to_numeric(e))),
																	  output->b_numeric)));
	switch (output->type)
	{
		case INT2OID:
			if (value < PG_INT16_MIN || value > PG_INT16_MAX)
				ereport(ERROR,
						(errcode(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
						 errmsg("smallint out of range")));
			return Int16GetDatum((int16) value);
		case INT4OID:
			if (value < PG_INT32_MIN || value > PG_INT32_MAX)
				ereport(ERROR,
						(errcode(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
						 errmsg("integer out of range")));
			return Int32GetDatum((int32) value);
		default:
			if (value < PG_INT64_MIN || value > PG_INT64_MAX)
				ereport(ERROR,
						(errcode(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
						 errmsg("bigint out of range")));
			return Int64GetDatum((int64) value);
	}
}
#endif

/* The checks of errors before an input: no row read. */
void
agg_check_reset(TessAggState *state)
{
	for (int number = 0; number < state->nchecks; number++)
		state->checks[number].seen = false;
}

/*
 * The least and the greatest of a column's values over the rows of a mask,
 * NULL skipped, into low and high, which hold those of the batches before
 * (low > high before any value): a word of 64 rows all read in one pass
 * without a branch a row, a NULL taking the extremes' neutral values; the
 * other words row by row.
 */
static inline void
word_extremes(const Datum *values, const bool *isnull, uint64 bits, bool wide,
			  int64 *low, int64 *high)
{
	int64		lo = *low;
	int64		hi = *high;

	if (bits == UINT64_MAX)
	{
		for (int row = 0; row < 64; row++)
		{
			int64		e = wide ? DatumGetInt64(values[row]) : (int64) DatumGetInt32(values[row]);

			lo = Min(lo, isnull[row] ? PG_INT64_MAX : e);
			hi = Max(hi, isnull[row] ? PG_INT64_MIN : e);
		}
	}
	else
	{
		while (bits != 0)
		{
			int			row = pg_rightmost_one_pos64(bits);
			int64		e = wide ? DatumGetInt64(values[row]) : (int64) DatumGetInt32(values[row]);

			bits &= bits - 1;
			if (isnull[row])
				continue;
			lo = Min(lo, e);
			hi = Max(hi, e);
		}
	}
	*low = lo;
	*high = hi;
}

/* The least and the greatest e of the rows of a batch a check's forms read. */
void
agg_check_take(TessAggState *state, const AggValue *value, const TessDatumColumn *column,
			   const TessRowMask *rows)
{
	AggCheck   *check = &state->checks[value->check];
	int64		low = check->seen ? check->low : PG_INT64_MAX;
	int64		high = check->seen ? check->high : PG_INT64_MIN;
	int			nwords = tess_row_mask_word_count(rows->nrows);

	for (int word = 0; word < nwords; word++)
	{
		if (check->wide)
			word_extremes(column->values + word * 64, column->isnull + word * 64,
						  rows->bits[word], true, &low, &high);
		else
			word_extremes(column->values + word * 64, column->isnull + word * 64,
						  rows->bits[word], false, &low, &high);
	}
	/* A value seen leaves low at most high; none leaves them as they started. */
	if (low <= high)
	{
		check->low = low;
		check->high = high;
		check->seen = true;
	}
}

/*
 * The end of the input: every form evaluated at the least and the greatest
 * e its rows had, by the core's executor, each step in its own type; the
 * error it raises is the query's, before any row goes out.
 */
void
agg_check_finish(TessAggState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;

	for (int number = 0; number < state->nchecks; number++)
	{
		AggCheck   *check = &state->checks[number];

		if (!check->seen)
			continue;
		for (int side = 0; side < 2; side++)
		{
			int64		e = side == 0 ? check->low : check->high;
			TupleTableSlot *slot = check->slot;

			ExecClearTuple(slot);
			slot->tts_values[0] = check->type == INT2OID ? Int16GetDatum((int16) e) :
				check->type == INT4OID ? Int32GetDatum((int32) e) : Int64GetDatum(e);
			slot->tts_isnull[0] = false;
			ExecStoreVirtualTuple(slot);
			econtext->ecxt_innertuple = slot;
			foreach_ptr(ExprState, probe, check->probes)
			{
				bool		isnull;

				(void) ExecEvalExprSwitchContext(probe, econtext, &isnull);
			}
			ResetExprContext(econtext);
		}
	}
}

static void
agg_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessAggState *state = (TessAggState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	TessRequest request = TESS_STRUCT_INITIALIZER(TessRequest);
	TessBuilderConfig builder = TESS_STRUCT_INITIALIZER(TessBuilderConfig);
	TupleTableSlot *result = css->ss.ps.ps_ResultTupleSlot;
	Plan	   *child_plan = linitial(cscan->custom_plans);
	Bitmapset  *projection = NULL;
	List	   *computed = NIL;
	List	   *arguments;
	List	   *more;
	List	   *filters;
	List	   *eqops;
	List	   *keys;
	List	   *states;
	List	   *outputs;
	List	   *derived;
	List	   *constants;
	List	   *checks;
	List	   *probes;
	TessPlanReader *reader;
	int			groups;
	int			groups_kb;
	int			index = 0;

	tess_node_require_forward(eflags, "TessAgg");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_agg_node || info.nchildren < 1 || info.nchildren > 2 ||
		info.child_names[0] == NULL || cscan->custom_scan_tlist == NIL)
		elog(ERROR, "TessAgg received a foreign plan");
	reader = tess_plan_reader_create((List *) info.node_data, TESS_AGG_DATA,
									 TESS_AGG_DATA_VERSION);
	states = tess_plan_read_list(reader, "states");
	outputs = tess_plan_read_int_list(reader, "outputs");
	derived = tess_plan_read_int_list(reader, "derived");
	constants = tess_plan_read_list(reader, "constants");
	checks = tess_plan_read_int_list(reader, "checks");
	probes = tess_plan_read_list(reader, "probes");
	arguments = tess_plan_read_list(reader, "arguments");
	more = tess_plan_read_list(reader, "more");
	filters = tess_plan_read_list(reader, "filters");
	eqops = tess_plan_read_int_list(reader, "key_eqops");
	keys = tess_plan_read_list(reader, "keys");
	groups = tess_plan_read_int(reader, "groups");
	state->groups_estimate = (uint64) Max(groups, 0);
	state->setop = tess_plan_read_int(reader, "setop");
	state->partial = tess_plan_read_int(reader, "partial") != 0;
	state->own_states = tess_plan_read_int(reader, "own_states") != 0;
	state->finalize = tess_plan_read_int(reader, "finalize") != 0;
	groups_kb = tess_plan_read_int(reader, "groups_kb");
	state->groups_bytes_estimate = (Size) Max(groups_kb, 0) * 1024;
	tess_plan_reader_finish(reader);
	if (state->finalize && (state->partial || state->setop >= 0))
		elog(ERROR, "TessAgg received a foreign plan");
	if (state->own_states && !state->partial && !(state->finalize && keys == NIL))
		elog(ERROR, "TessAgg received a foreign plan");
	if ((state->setop >= 0) != (info.nchildren == 2) ||
		(state->setop >= 0 && (list_length(arguments) != 2 || list_length(keys) == 0)))
		elog(ERROR, "TessAgg received a foreign plan");
	state->nkeys = list_length(keys);
	if (state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(outputs) + state->nkeys != list_length(cscan->custom_scan_tlist) ||
		list_length(states) != list_length(arguments) ||
		(state->nkeys == 0 && arguments == NIL) ||
		list_length(filters) != list_length(arguments) ||
		list_length(more) != list_length(arguments) ||
		list_length(eqops) != state->nkeys ||
		list_length(derived) % AGG_DERIVED_STATES != 0 ||
		list_length(constants) != list_length(derived) / AGG_DERIVED_STATES * AGG_DERIVED_CONSTANTS)
		elog(ERROR, "TessAgg received a foreign plan");
	state->noutputs = list_length(outputs);
	state->child = ExecInitNode(child_plan, estate, eflags);
	css->custom_ps = list_make1(state->child);
	state->input = tess_input_create(estate->es_query_cxt, state->child);
	state->child_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	tess_plan_get_layout(child_plan, &state->child_layout);
	if (state->setop >= 0)
	{
		Plan	   *right = lsecond(cscan->custom_plans);

		state->sides[0] = state->child;
		state->side_inputs[0] = state->input;
		state->side_layouts[0] = state->child_layout;
		state->sides[1] = ExecInitNode(right, estate, eflags);
		css->custom_ps = lappend(css->custom_ps, state->sides[1]);
		state->side_inputs[1] = tess_input_create(estate->es_query_cxt, state->sides[1]);
		state->side_layouts[1] = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
		tess_plan_get_layout(right, &state->side_layouts[1]);
		state->setop_copies = -1;
	}
	state->nvalues = list_length(arguments);
	state->values = palloc0_array(AggValue, Max(state->nvalues, 1));
	state->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	/* The keys are the first computed columns, the table's key kinds. */
	foreach_ptr(Node, key, keys)
	{
		int			position = foreach_current_index(key);

		state->key_bytes_column[position] = -1;
		if (list_nth_int(eqops, position) != 0)
		{
			Node	   *bare = key;

			state->kinds[position] = TESS_KEY_KIND_NUMBER;
			state->dicts[position] = agg_key_dict_create(state, (Oid) list_nth_int(eqops, position),
													exprType(key), exprCollation(key));
			state->has_dicts = true;
			state->has_forms |= state->dicts[position]->forms;
			state->row_spill = true;
			/*
			 * A key of bytes that is a column of the child, varchar as text
			 * among them, may come in the bytes form; not of a set
			 * operation, whose two sides have two layouts.
			 */
			while (IsA(bare, RelabelType))
				bare = (Node *) ((RelabelType *) bare)->arg;
			if (state->dicts[position]->bytes == TESS_KEY_BYTES_WHOLE && state->setop < 0 &&
				IsA(bare, Var) && ((Var *) bare)->varno == INDEX_VAR)
				state->key_bytes_column[position] =
					tess_layout_column(&state->child_layout, ((Var *) bare)->varattno - 1);
		}
		else if (!tess_word_key_kind(exprType(key), &state->kinds[position]))
			elog(ERROR, "TessAgg received a key of type %u", exprType(key));
		computed = lappend(computed,
						   makeTargetEntry((Expr *) key, position + 1, NULL, false));
		foreach_ptr(Var, var, pull_var_clause(key, 0))
		{
			int			column = var->varno == INDEX_VAR ?
				tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

			if (column < 0)
				elog(ERROR, "TessAgg grouping expression names no column of its child");
			projection = bms_add_member(projection, column);
		}
	}
	foreach_node(Aggref, agg, states)
	{
		AggValue   *value = &state->values[index++];
		TessTableKeyKind kind;

		/* The partial values are the whole ones' types: nothing to convert. */
		if (DO_AGGSPLIT_SKIPFINAL(agg->aggsplit) != state->partial)
			elog(ERROR, "TessAgg received a foreign plan");
		value->kind = aggregate_kind(agg->aggfnoid);
		/* An extreme's width is its word's, by the type of its value. */
		value->wide = tess_word_key_kind(agg->aggtranstype, &kind) &&
			tess_key_kind_wide(kind);
		value->function = tess_runtime_api()->functions->find(agg->aggfnoid);
		value->computed = -1;
		value->filter = -1;
		value->distinct_bytes_column = -1;
		value->check = -1;
		if (!batch_aggregate(agg))
		{
			if (!generic_supported(agg) ||
				(state->nkeys > 0 && DO_AGGSPLIT_SKIPFINAL(agg->aggsplit) &&
				 !(state->own_states && sum_state_aggregate(agg))))
				elog(ERROR, "TessAgg has no implementation of %s",
					 format_procedure(agg->aggfnoid));
			value->kind = AGG_GENERIC;
			value->generic = agg_generic_init(state, agg);
			if (state->nkeys > 0 && state->generic_output == NULL)
			{
				state->has_generic = true;
				state->row_spill = true;
				state->generic_output = AllocSetContextCreate(estate->es_query_cxt,
															  "TessAgg generic values",
															  ALLOCSET_DEFAULT_SIZES);
			}
		}
		/*
		 * A partial value is the transition type's: an extreme's, int8 for
		 * the rest; with groups a sum state's the node's own
		 * (TessTableSumInput), without them a generic one's the core's,
		 * which its combine function takes.
		 */
		if (state->finalize && ((value->kind == AGG_GENERIC && state->nkeys > 0 &&
								 !value->generic->sum_state) ||
								distinct_matters(agg)))
			elog(ERROR, "TessAgg received a foreign plan");
		if (state->partial && value->kind == AGG_GENERIC && state->nkeys > 0 &&
			!value->generic->sum_state)
			elog(ERROR, "TessAgg received a foreign plan");
		switch (value->kind)
		{
			case AGG_COUNT:
				value->accumulate = state->finalize ? TESS_TABLE_SUM_INT8 :
					agg->args == NIL ? TESS_TABLE_COUNT_ROWS : TESS_TABLE_COUNT;
				break;
			case AGG_SUM:
				value->accumulate = state->finalize ? TESS_TABLE_SUM_INT8 :
					TESS_TABLE_SUM_INT4;
				break;
			case AGG_MIN:
				value->accumulate = value->wide ? TESS_TABLE_MIN_INT8 :
					TESS_TABLE_MIN_INT4;
				break;
			case AGG_MAX:
				value->accumulate = value->wide ? TESS_TABLE_MAX_INT8 :
					TESS_TABLE_MAX_INT4;
				break;
			case AGG_GENERIC:
				break;
		}
		if (agg->args != NIL || state->finalize)
		{
			Node	   *argument = list_nth(arguments, index - 1);
			List	   *vars = pull_var_clause(argument, 0);

			value->computed = -1;
			/*
			 * An argument equal to an earlier state's, with no volatile
			 * function, its FILTER's among them, is that state's computed
			 * column: its values are the same, computed once. A volatile one
			 * is computed for each, as the core computes it for each, and an
			 * argument followed by more has its own, which they follow; a
			 * key's column is not shared, as its values may become numbers.
			 */
			if (list_nth(more, index - 1) == NIL && !contain_volatile_functions(argument))
			{
				foreach_node(TargetEntry, earlier, computed)
				{
					if (foreach_current_index(earlier) >= state->nkeys &&
						equal(earlier->expr, argument))
					{
						value->computed = foreach_current_index(earlier);
						break;
					}
				}
			}
			if (value->computed < 0)
			{
				value->computed = list_length(computed);
				computed = lappend(computed,
								   makeTargetEntry((Expr *) argument,
												   value->computed + 1, NULL, false));
			}
			/* The other arguments of a generic aggregate follow the first. */
			foreach_ptr(Node, other, (List *) list_nth(more, index - 1))
			{
				computed = lappend(computed,
								   makeTargetEntry((Expr *) other, list_length(computed) + 1,
												   NULL, false));
				vars = list_concat(vars, pull_var_clause(other, 0));
			}
			foreach_ptr(Var, var, vars)
			{
				int			column = var->varno == INDEX_VAR ?
					tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

				if (column < 0)
					elog(ERROR, "TessAgg argument names no column of its child");
				projection = bms_add_member(projection, column);
			}
			value->gathered_values = palloc_array(Datum, 64);
			value->gathered_isnull = palloc_array(bool, 64);
			if (distinct_matters(agg))
			{
				SortGroupClause *clause = linitial_node(SortGroupClause, agg->aggdistinct);

				if (!tess_word_key_kind(exprType(argument), &value->argument_kind))
				{
					value->argument_kind = TESS_KEY_KIND_NUMBER;
					value->distinct_dict = agg_key_dict_create(state, clause->eqop,
														   exprType(argument),
														   exprCollation(argument));
					/* Only the count of the values matters, not a form to put out. */
					value->distinct_dict->forms = false;
					/*
					 * An ungrouped count of a bare column of bytes: by its
					 * bytes where the batch offers them, as a key's.
					 */
					if (state->nkeys == 0 && value->kind == AGG_COUNT &&
						value->distinct_dict->bytes == TESS_KEY_BYTES_WHOLE)
					{
						Node	   *bare = argument;

						while (IsA(bare, RelabelType))
							bare = (Node *) ((RelabelType *) bare)->arg;
						if (IsA(bare, Var) && ((Var *) bare)->varno == INDEX_VAR)
						{
							value->distinct_bytes_column =
								tess_layout_column(&state->child_layout,
												   ((Var *) bare)->varattno - 1);
							state->has_distinct_bytes = true;
						}
					}
				}
				value->distinct = palloc0(sizeof(struct DistinctSet));
				state->has_distinct = true;
				/* Its pairs go to disk past hash_mem, with GROUP BY and a word. */
				if (state->nkeys > 0 && value->distinct_dict == NULL)
				{
					value->distinct->pairs = palloc0(sizeof(PairSpill));
					state->spilling_sets++;
				}
			}
		}
	}
	derived_outputs(state, cscan, states, outputs, derived, constants);
	init_checks(state, estate, arguments, checks, probes);
	/* The FILTER conditions, computed columns after every argument. */
	foreach_ptr(Node, filter, filters)
	{
		AggValue   *value = &state->values[foreach_current_index(filter)];

		if (filter == NULL)
			continue;
		value->filter = list_length(computed);
		computed = lappend(computed,
						   makeTargetEntry((Expr *) filter, value->filter + 1, NULL, false));
		foreach_ptr(Var, var, pull_var_clause(filter, 0))
		{
			int			column = var->varno == INDEX_VAR ?
				tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

			if (column < 0)
				elog(ERROR, "TessAgg filter names no column of its child");
			projection = bms_add_member(projection, column);
		}
	}
	state->ncomputed = list_length(computed);
	/* The planner's bound: the computed columns within a tuple's, as a spill of rows keeps them. */
	if (state->ncomputed > TESS_ROWS_MAX_COLUMNS)
		elog(ERROR, "TessAgg received a foreign plan");
	state->computed_lens = palloc_array(int16, Max(state->ncomputed, 1));
	state->computed_byvals = palloc_array(bool, Max(state->ncomputed, 1));
	state->computed_columns = palloc0_array(TessDatumColumn, Max(state->ncomputed, 1));
	foreach_node(TargetEntry, entry, computed)
	{
		int			column = foreach_current_index(entry);

		get_typlenbyval(exprType((Node *) entry->expr), &state->computed_lens[column],
						&state->computed_byvals[column]);
	}
	if (computed != NIL)
	{
		/*
		 * The arguments are computed columns over the child's target list,
		 * in a slot of its own: the node's scan tuple is the aggregates'.
		 */
		state->projection =
			tess_node_projection(css,
								 ExecInitExtraTupleSlot(estate,
														ExecTypeFromTL(child_plan->targetlist),
														&TTSOpsVirtual),
								 &state->child_layout, state->child_layout.ncolumns, computed);
		/*
		 * The right side: the same keys, its columns by position, and its
		 * side a constant 1 where the left side's is 0.
		 */
		if (state->setop >= 0)
		{
			List	   *right = copyObject(computed);
			TargetEntry *side = list_nth_node(TargetEntry, right, state->values[1].computed);
			Plan	   *plan = lsecond(cscan->custom_plans);
			Bitmapset  *columns = NULL;
			TessRequest side_request = TESS_STRUCT_INITIALIZER(TessRequest);

			if (!IsA(side->expr, Const))
				elog(ERROR, "TessAgg received a foreign plan");
			side->expr = (Expr *) makeConst(INT4OID, -1, InvalidOid, sizeof(int32),
											Int32GetDatum(1), false, true);
			state->side_projections[0] = state->projection;
			state->side_projections[1] =
				tess_node_projection(css,
									 ExecInitExtraTupleSlot(estate,
															ExecTypeFromTL(plan->targetlist),
															&TTSOpsVirtual),
									 &state->side_layouts[1], state->side_layouts[1].ncolumns,
									 right);
			foreach_node(TargetEntry, entry, right)
				foreach_node(Var, var, pull_var_clause((Node *) entry->expr, 0))
				{
					int			column = var->varno == INDEX_VAR ?
						tess_layout_column(&state->side_layouts[1], var->varattno - 1) : -1;

					if (column < 0)
						elog(ERROR, "TessAgg key names no column of its right side");
					columns = bms_add_member(columns, column);
				}
			side_request.projection_columns = columns;
			side_request.output_mode = TESS_OUTPUT_BATCH;
			tess_input_set_request(state->side_inputs[1], &side_request);
		}
	}
	/* Whole batches; the arguments' columns only for the surviving rows. */
	request.projection_columns = projection;
	/*
	 * Read in order only when the keys and the arguments, as computed, would
	 * first ask for a column before one they asked for already: else the
	 * provider walks each row once anyway, and the calls are wasted.
	 */
	{
		int			last = -1;
		bool		ascending = true;

		foreach_node(TargetEntry, entry, computed)
			foreach_node(Var, var, pull_var_clause((Node *) entry->expr, 0))
			{
				int			column = var->varno == INDEX_VAR ?
					tess_layout_column(&state->child_layout, var->varattno - 1) : -1;

				if (column >= 0 && column < last)
					ascending = false;
				last = Max(last, column);
			}
		state->read_columns = palloc_array(int, Max(bms_num_members(projection), 1));
		/* Two sides of two layouts: their columns come as they are asked for. */
		for (int column = -1;
			 !ascending && state->setop < 0 &&
			 (column = bms_next_member(projection, column)) >= 0;)
			state->read_columns[state->nread_columns++] = column;
	}
	/*
	 * The states' places in a record, after its words of flags: a word
	 * each, a sum or extreme state's more.
	 */
	{
		int			slot = AGG_FLAG_WORDS(state->nvalues);

		state->nflag_words = slot;
		state->flag_words = palloc0_array(uint64, slot * AGG_GROUP_ROWS);

		for (int number = 0; number < state->nvalues; number++)
		{
			AggValue   *value = &state->values[number];

			value->slot = slot;
			slot += value->generic == NULL ? 1 :
				value->generic->sum_state ? AGG_SUM_STATE_WORDS :
				value->generic->extreme_state ? AGG_EXTREME_STATE_WORDS : 1;
		}
		state->payload_size = sizeof(uint64) * slot;
		state->sum_indexes = palloc_array(int, Max(state->nvalues, 1));
	}
	request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->input, &request);
	/* Sparse input batches come gathered (runtime_input.h), both sides'. */
	tess_input_set_gather(state->input, true);
	if (state->setop >= 0)
		tess_input_set_gather(state->side_inputs[1], true);
	builder.parent_context = estate->es_query_cxt;
	builder.tuple_desc = result->tts_tupleDescriptor;
	builder.ncolumns = result->tts_tupleDescriptor->natts;
	builder.capacity = state->nkeys > 0 ? AGG_GROUP_ROWS : 1;
	if (state->nkeys > 0)
	{
		state->kernels = tess_runtime_kernels();
		if (state->kernels == NULL)
			elog(ERROR, "TessAgg needs the kernels module for GROUP BY");
		check(state, state->kernels->table_record_size(state->nkeys, state->payload_size,
													   &state->record_size,
													   &state->status));
		/* The planner's bound: a record within the first chunk. */
		if (state->record_size > TABLE_FIRST_CHUNK - TESS_TABLE_CHUNK_HEADER)
			elog(ERROR, "TessAgg received a foreign plan");
		state->table_context = AllocSetContextCreate(estate->es_query_cxt,
													 "TessAgg groups",
													 ALLOCSET_DEFAULT_SIZES);
		/* For spilling: how the states merge, and the layout's fingerprint. */
		state->combines = palloc_array(TessTableCombine, Max(state->nvalues, 1));
		for (int value = 0; value < state->nvalues; value++)
			state->combines[value] =
				state->values[value].kind == AGG_COUNT ? TESS_TABLE_COMBINE_COUNT :
				state->values[value].kind == AGG_SUM ? TESS_TABLE_COMBINE_SUM :
				state->values[value].kind == AGG_MIN ? TESS_TABLE_COMBINE_MIN :
				TESS_TABLE_COMBINE_MAX;
		if (state->kernels->table_size(state->nkeys,
									   state->payload_size,
									   AGG_INITIAL_GROUPS, &state->layout_len,
									   &state->status) != TESS_OK)
			tess_status_report(&state->status);
		state->layout_index = palloc0(state->layout_len);
		if (state->kernels->table_create(state->layout_index, state->layout_len,
										 state->nkeys,
										 state->payload_size,
										 AGG_INITIAL_GROUPS, &state->status) != TESS_OK)
			tess_status_report(&state->status);
		state->state_words = palloc0_array(uint64,
										   Max(state->nvalues, 1) * AGG_GROUP_ROWS);
		if (state->projection == NULL)
			elog(ERROR, "TessAgg groups by computed columns");
	}
	/* DISTINCT in an aggregate keeps its pairs in tables, with or without groups. */
	if (state->has_distinct && state->kernels == NULL)
	{
		state->kernels = tess_runtime_kernels();
		if (state->kernels == NULL)
			elog(ERROR, "TessAgg needs the kernels module for DISTINCT");
	}
	state->builder = tess_builder_create(&builder);
	/* A derived numeric lives as a generic value does, until the next group. */
	if (state->nkeys > 0 && state->derived_by_ref && state->generic_output == NULL)
		state->generic_output = AllocSetContextCreate(estate->es_query_cxt,
													  "TessAgg generic values",
													  ALLOCSET_DEFAULT_SIZES);
	if (state->nkeys > 0 && state->setop < 0 && !state->has_generic &&
		!state->derived_by_ref &&
		css->ss.ps.qual == NULL && css->ss.ps.ps_ProjInfo == NULL)
	{
		state->direct = true;
		state->agg_values = palloc0_array(Datum, Max(state->noutputs, 1) * AGG_GROUP_ROWS);
		state->agg_isnull = palloc0_array(bool, Max(state->noutputs, 1) * AGG_GROUP_ROWS);
		state->groups_batch.abi_version = TESS_BATCH_ABI_VERSION;
		state->groups_batch.struct_size = sizeof(TessBatch);
		state->groups_batch.values_stored = true;
		state->groups_batch.table_oid = InvalidOid;
		state->groups_batch.ops = &groups_batch_ops;
		state->groups_batch.private_data = state;
	}
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   result, &info.layout);
}

/*
 * A partial value into the running value, a batch's or, above a gather, a
 * participant's: counts and sums added as int8, 22003 past the range,
 * extremes compared.
 */
static void
join_partial(AggValue *value, Datum partial)
{
	switch (value->kind)
	{
		case AGG_COUNT:
		case AGG_SUM:
			if (pg_add_s64_overflow(value->total, DatumGetInt64(partial),
									&value->total))
				ereport(ERROR,
						(errcode(ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE),
						 errmsg("bigint out of range")));
			break;
		case AGG_MIN:
		case AGG_MAX:
			{
				int64		found = value->wide ? DatumGetInt64(partial) :
					(int64) DatumGetInt32(partial);

				if (!value->has_value ||
					(value->kind == AGG_MIN ? found < value->extreme :
					 found > value->extreme))
					value->extreme = found;
				break;
			}
		case AGG_GENERIC:
			break;
	}
	value->has_value = true;
}

/*
 * One call of the aggregate's batch function over a column, or over rows
 * alone for count(*); the partial joins the running value. No readiness
 * mask: the column's rows are all initialized memory (tessera/batch.h),
 * and a mask would keep the kernel off its vector path for every word the
 * selection does not fill.
 */
static void
evaluate(TessAggState *state, AggValue *value, const TessDatumColumn *column,
		 TessRowMask *rows)
{
	TessFunctionCall call = TESS_STRUCT_INITIALIZER(TessFunctionCall);
	TessFunctionArg arg = TESS_STRUCT_INITIALIZER(TessFunctionArg);
	uint64		word = 0;
	TessRowMask present = {1, &word};
	Datum		partial = (Datum) 0;

	call.function = value->function;
	if (column != NULL)
	{
		arg.column = column;
		call.nargs = 1;
		call.args = &arg;
	}
	call.rows = rows;
	call.values = &partial;
	call.non_nulls = &present;
	call.context = CurrentMemoryContext;
	call.status = &state->status;
	state->calls++;
	if (value->function->evaluate(&call) != TESS_OK)
		tess_status_report(&state->status);
	if ((word & 1) == 0)
		return;
	join_partial(value, partial);
}

/* The gathered values as a column with every row selected, in one call. */
static void
flush_gathered(TessAggState *state, AggValue *value)
{
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	uint64		word;
	TessRowMask rows = {value->ngathered, &word};

	if (value->ngathered == 0)
		return;
	column.values = value->gathered_values;
	column.isnull = value->gathered_isnull;
	column.nrows = value->ngathered;
	word = value->ngathered == 64 ? UINT64_MAX :
		(UINT64CONST(1) << value->ngathered) - 1;
	evaluate(state, value, &column, &rows);
	value->ngathered = 0;
}

/*
 * The rows of those given that an aggregate's FILTER keeps: its computed
 * column is true there and NULL elsewhere (filter_value), asked for those
 * rows only; in the node's buffer, which the aggregate uses before the
 * next one asks.
 */
TessRowMask
agg_filtered_rows(TessAggState *state, TessBatch *batch, int filter, const TessRowMask *rows)
{
	TessDatumColumn column = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	int			nwords = tess_row_mask_word_count(rows->nrows);
	int			row = -1;

	if (state->filter_words < nwords)
	{
		state->filter_bits = MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
												sizeof(uint64) * nwords);
		state->filter_words = nwords;
	}
	batch->ops->get_datum_column(batch, state->child_layout.ncolumns + filter, rows,
								 TESS_COLUMN_FOR_FILTER, &column);
	if (column.values == NULL || column.isnull == NULL || column.nrows != rows->nrows)
		elog(ERROR, "Tessera projection returned an invalid column");
	for (int word = 0; word < nwords; word++)
	{
		uint64		selected = rows->bits[word];
		uint64		kept = 0;
		int			base = word * 64;

		/* A whole word without a branch a row; a partial one row by row. */
		if (selected == UINT64_MAX)
		{
			for (int bit = 0; bit < 64; bit++)
				kept |= (uint64) ((!column.isnull[base + bit]) &
								  (DatumGetBool(column.values[base + bit]) ? 1 : 0)) << bit;
		}
		else
		{
			while (selected != 0)
			{
				int			bit = pg_rightmost_one_pos64(selected);

				row = base + bit;
				if (!column.isnull[row] && DatumGetBool(column.values[row]))
					kept |= UINT64CONST(1) << bit;
				selected &= selected - 1;
			}
		}
		state->filter_bits[word] = kept;
	}
	return (TessRowMask) {rows->nrows, state->filter_bits};
}

/*
 * Add one batch to the aggregate: its partial through the batch function,
 * or, for a batch with few survivors, their values gathered into a column
 * of the aggregate's own, since a call costs more than the rows it would
 * sum; the column is evaluated when it fills or the input ends. The batch
 * is the projection's wrapper, which computes the argument's column.
 */
static void
accumulate(TessAggState *state, AggValue *value, TessBatch *batch, int nrows)
{
	TessDatumColumn computed = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	const TessDatumColumn *column = &computed;
	TessRowMask rows = batch->rows;
	int			row = -1;

	if (value->filter >= 0)
	{
		rows = agg_filtered_rows(state, batch, value->filter, &batch->rows);
		nrows = tess_row_mask_count(&rows);
	}
	if (value->computed < 0)
	{
		evaluate(state, value, NULL, &rows);
		return;
	}
	if (value->distinct_by_bytes)
	{
		/* Numbered by its bytes below: no Datum of the argument made. */
		computed.nrows = batch->rows.nrows;
		computed.isnull = value->distinct_bytes.isnull;
	}
	else
	{
		/* A numeric aggregate of its own reads its argument's decimals. */
		computed.accept_decimals = fast_decimals(value);
		batch->ops->get_datum_column(batch,
									 state->child_layout.ncolumns + value->computed,
									 &rows, TESS_COLUMN_FOR_PROJECTION, &computed);
		if (computed.values == NULL || computed.isnull == NULL ||
			computed.nrows != batch->rows.nrows)
			elog(ERROR, "Tessera projection returned an invalid column");
	}
	if (value->check >= 0)
		agg_check_take(state, value, &computed, &rows);
	/*
	 * Above a gather: a row a participant's partial value, NULL skipped (a
	 * participant without rows gives a count of 0 and NULL otherwise), not
	 * the rows a batch function would count.
	 */
	if (state->finalize)
	{
#ifdef HAVE_INT128
		if (value->kind == AGG_GENERIC && value->generic->fast != FAST_NONE)
		{
			agg_fast_merge(state, value->generic, &computed, &rows);
			return;
		}
#endif
		while ((row = tess_row_mask_next(&rows, row)) >= 0)
		{
			if (value->kind == AGG_GENERIC)
				agg_generic_combine(state, value->generic, computed.values[row],
								computed.isnull[row]);
			else if (!computed.isnull[row])
				join_partial(value, computed.values[row]);
		}
		return;
	}
	if (value->generic != NULL)
	{
		value->generic->columns[0] = computed;
		for (int arg = 1; arg < value->generic->nargs; arg++)
		{
			TessDatumColumn *other = &value->generic->columns[arg];

			*other = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
			batch->ops->get_datum_column(batch,
										 state->child_layout.ncolumns + value->computed + arg,
										 &rows, TESS_COLUMN_FOR_PROJECTION, other);
			if (other->values == NULL || other->isnull == NULL ||
				other->nrows != batch->rows.nrows)
				elog(ERROR, "Tessera projection returned an invalid column");
		}
		if (value->distinct != NULL)
		{
			TessRowMask pairs = agg_distinct_rows(state, value, rows.nrows, NULL, NULL, &rows,
												  column);

			agg_generic_accumulate(state, value->generic, &pairs);
		}
		else
			agg_generic_accumulate(state, value->generic, &rows);
		return;
	}
	if (value->distinct != NULL)
	{
		TessRowMask pairs = agg_distinct_rows(state, value, rows.nrows, NULL, NULL, &rows,
											  column);

		/* The count reads the rows' NULLs; their numbers stand for the values. */
		if (value->distinct_by_bytes)
			computed.values = value->distinct_dict->batch_numbers;
		evaluate(state, value, column, &pairs);
		return;
	}
	if (nrows > AGG_GATHER_ROWS)
	{
		evaluate(state, value, column, &rows);
		return;
	}
	while ((row = tess_row_mask_next(&rows, row)) >= 0)
	{
		if (value->ngathered == 64)
			flush_gathered(state, value);
		value->gathered_values[value->ngathered] = column->values[row];
		value->gathered_isnull[value->ngathered] = column->isnull[row];
		value->ngathered++;
	}
}

/*
 * Empty every distinct set, before the input is read, the pairs of the
 * last input on disk forgotten; their first indexes noted.
 */
void
agg_reset_distinct(TessAggState *state)
{
	if (state->spilling_sets > 0)
		pairs_params_init(state, state->spilling_sets);
	for (int index = 0; index < state->nvalues; index++)
	{
		DistinctSet *set = state->values[index].distinct;

		if (set == NULL)
			continue;
		agg_distinct_reset(state, &state->values[index]);
		if (set->pairs != NULL)
		{
			pairs_free(set->pairs);
			pairs_begin(state, set->pairs);
		}
	}
	if (state->has_distinct)
		note_memory(state);
}

/* Forget the pairs of the distinct sets on disk. */
static void
free_pairs(TessAggState *state)
{
	for (int index = 0; index < state->nvalues; index++)
		if (state->values[index].distinct != NULL &&
			state->values[index].distinct->pairs != NULL)
			pairs_free(state->values[index].distinct->pairs);
}

/* Read every batch of the child into the running values. */
static void
drain(TessAggState *state)
{
	agg_reset_distinct(state);
	agg_check_reset(state);
	/* The states of a previous scan go, with the callbacks they registered. */
	if (state->generic_agg != NULL)
		ReScanExprContext(state->generic_agg->curaggcontext);
	for (int index = 0; index < state->nvalues; index++)
		if (state->values[index].generic != NULL)
			agg_generic_reset(state, state->values[index].generic);
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->input);
		int			rows;

		if (batch == NULL)
			break;
		rows = tess_row_mask_count(&batch->rows);
		state->batches++;
		state->rows += rows;
		if (rows > 0)
		{
			TessBatch  *input = batch;

			ResetExprContext(state->css.ss.ps.ps_ExprContext);
			if (state->projection != NULL)
				input = tess_projection_wrap(state->projection, batch);
			/* A distinct argument by its bytes, where the batch gives them. */
			for (int index = 0; state->has_distinct_bytes && index < state->nvalues; index++)
			{
				AggValue   *value = &state->values[index];

				value->distinct_by_bytes = value->distinct_bytes_column >= 0 &&
					tess_batch_bytes_column(input, value->distinct_bytes_column,
											&input->rows, &value->distinct_bytes);
			}
			agg_read_in_order(state, input);
			for (int index = 0; index < state->nvalues; index++)
				accumulate(state, &state->values[index], input, rows);
			if (state->projection != NULL)
				input->ops->release(input);
		}
		tess_input_finish(state->input);
	}
	for (int index = 0; index < state->nvalues; index++)
		flush_gathered(state, &state->values[index]);
	agg_check_finish(state);
}

/* A state's value without GROUP BY. */
static Datum
plain_value(TessAggState *state, int index, bool *isnull)
{
	AggValue   *value = &state->values[index];

	*isnull = !value->has_value;
	switch (value->kind)
	{
		case AGG_COUNT:
			*isnull = false;
			return Int64GetDatum(value->total);
		case AGG_SUM:
			return Int64GetDatum(value->total);
		case AGG_MIN:
		case AGG_MAX:
			return value->wide ? Int64GetDatum(value->extreme) :
				Int32GetDatum((int32) value->extreme);
		case AGG_GENERIC:
			return agg_generic_value(value->generic, isnull);
	}
	return (Datum) 0;
}

/* The one result row: the aggregates in the scan slot, each its output's value. */
static TupleTableSlot *
result_row(TessAggState *state)
{
	TupleTableSlot *scan = state->css.ss.ss_ScanTupleSlot;

	ExecClearTuple(scan);
	for (int number = 0; number < state->noutputs; number++)
	{
		const AggOutput *output = &state->outputs[number];

		if (output->state >= 0)
			scan->tts_values[number] = plain_value(state, output->state,
												   &scan->tts_isnull[number]);
		else
		{
#ifdef HAVE_INT128
			AggValue   *value = &state->values[output->source];
			AggSum		sum = {0};

			if (output->kind == AGG_DERIVED_MIN || output->kind == AGG_DERIVED_MAX)
			{
				Datum		extreme = plain_value(state, output->source,
												  &scan->tts_isnull[number]);

				/* Of a constant: the count of rows, 0 none, the form at e = 0. */
				if (value->kind == AGG_COUNT)
					scan->tts_isnull[number] = value->total == 0;
				scan->tts_values[number] =
					agg_derive_extreme(output, extreme, value->kind == AGG_GENERIC,
									   value->kind == AGG_GENERIC || value->kind == AGG_COUNT ?
									   0 : value->extreme,
									   &scan->tts_isnull[number]);
				continue;
			}
			/*
			 * A sum state, of avg(e) of bigint or numeric, or the words of
			 * sum(e); a constant's count of rows, no e.
			 */
			if (value->kind == AGG_GENERIC)
				agg_fast_sum(value->generic, &sum);
			else if (value->kind == AGG_SUM)
				sum.sum = value->total;
			if (output->count_state >= 0)
				sum.count = state->values[output->count_state].total;
			scan->tts_values[number] = agg_derive(output, &sum, &scan->tts_isnull[number]);
#endif
		}
	}
	return ExecStoreVirtualTuple(scan);
}

/*
 * A column of the groups' own batch: a key's values as the walk gathered
 * them (a number already its value, which lives in the dictionary until
 * the next partition's rows are read, after the batch is done with), or an
 * aggregate's values.
 */
static void
groups_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				  TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessAggState *state = (TessAggState *) batch->private_data;

	if (column < 0 || column >= state->nkeys + state->noutputs)
		elog(ERROR, "TessAgg has no column %d", column);
	if (column < state->nkeys)
	{
		result->values = state->key_values[column];
		result->isnull = state->key_isnull[column];
	}
	else
	{
		result->values = &state->agg_values[(column - state->nkeys) * AGG_GROUP_ROWS];
		result->isnull = &state->agg_isnull[(column - state->nkeys) * AGG_GROUP_ROWS];
	}
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps groups_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = groups_get_column,
};

/*
 * GROUP BY: after the input, a batch of groups per call to a batch-aware
 * parent, or their rows one by one to a row-wise parent.
 */
static TupleTableSlot *
group_exec(TessAggState *state)
{
	bool		rows = tess_output_request(state->output)->output_mode ==
		TESS_OUTPUT_ROWS;

	if (!state->drained)
		agg_group_drain(state);
	if (rows && state->published != NULL)
	{
		state->next_row = tess_row_mask_next(&state->published->rows,
											 state->next_row);
		if (state->next_row >= 0)
			return tess_output_select(state->output, state->next_row);
		tess_output_finish(state->output);
	}
	tess_output_release(state->output);
	state->published = agg_next_groups(state);
	if (state->published == NULL)
	{
		state->done = true;
		return NULL;
	}
	state->next_row = tess_row_mask_next(&state->published->rows, -1);
	return tess_output_publish(state->output, state->published);
}

/*
 * The result row, once: the aggregates in the scan slot, HAVING over
 * them, and the plan's projection when the targets are not the bare
 * aggregates, as the executor set it up for the scan tuple.
 */
static TupleTableSlot *
agg_exec(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;
	ExprContext *econtext = css->ss.ps.ps_ExprContext;
	TupleTableSlot *row;
	TessBatch  *batch;

	if (state->nkeys > 0)
		return state->done ? NULL : group_exec(state);
	if (state->done)
	{
		/* Served to a row-wise parent, or read by a batch-aware one. */
		tess_output_finish(state->output);
		tess_output_release(state->output);
		return NULL;
	}
	drain(state);
	state->done = true;
	ResetExprContext(econtext);
	/*
	 * The row's values, a derivation's numerics and their temporaries among
	 * them, in the row's memory: the builder copies the row below, and a
	 * rescan makes another without keeping the last.
	 */
	{
		MemoryContext old = MemoryContextSwitchTo(econtext->ecxt_per_tuple_memory);

		econtext->ecxt_scantuple = result_row(state);
		MemoryContextSwitchTo(old);
	}
	if (css->ss.ps.qual != NULL && !ExecQual(css->ss.ps.qual, econtext))
		return NULL;
	row = css->ss.ps.ps_ProjInfo != NULL ? ExecProject(css->ss.ps.ps_ProjInfo) :
		ExecCopySlot(css->ss.ps.ps_ResultTupleSlot, econtext->ecxt_scantuple);
	tess_builder_reset(state->builder);
	tess_builder_append_slot(state->builder, row);
	batch = tess_builder_finish(state->builder, InvalidOid);
	return tess_output_publish(state->output, batch);
}

static void
agg_end(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	if (state->setop >= 0)
	{
		ExecEndNode(state->sides[0]);
		ExecEndNode(state->sides[1]);
	}
	else
		ExecEndNode(state->child);
	agg_spill_free(state);
	rows_spill_free(state);
	free_pairs(state);
	if (state->table_context != NULL)
		MemoryContextDelete(state->table_context);
}

static void
agg_rescan(CustomScanState *css)
{
	TessAggState *state = (TessAggState *) css;

	tess_output_clear(state->output);
	/* INTERSECT or EXCEPT: both sides again, from the left. */
	if (state->setop >= 0)
	{
		for (int side = 0; side < 2; side++)
		{
			tess_projection_reset(state->side_projections[side]);
			tess_rescan_child(&css->ss.ps, state->sides[side], state->side_inputs[side]);
		}
		agg_setop_side(state, 0);
		state->setop_count = 0;
		state->setop_group = 0;
		state->setop_copies = -1;
	}
	else
	{
		if (state->projection != NULL)
			tess_projection_reset(state->projection);
		tess_rescan_child(&css->ss.ps, state->child, state->input);
	}
	for (int index = 0; index < state->nvalues; index++)
	{
		state->values[index].total = 0;
		state->values[index].has_value = false;
		state->values[index].ngathered = 0;
	}
	state->done = false;
	/* GROUP BY: the table is built again from the rescanned child. */
	agg_spill_free(state);
	rows_spill_free(state);
	free_pairs(state);
	state->drained = false;
	state->input_done = false;
	state->published = NULL;
	/*
	 * EXPLAIN's counters total every scan, as the core's instrumentation
	 * does; the early emits count this scan's rows from here.
	 */
	state->emit_rows = state->rows;
	state->partial_spill = false;
}

/* This participant's counters. */
static void
agg_counters(TessAggState *state, uint64 *values)
{
	memset(values, 0, AGG_NCOUNTERS * sizeof(uint64));
	values[AGG_BATCHES] = state->batches;
	values[AGG_ROWS] = state->rows;
	values[AGG_CALLS] = state->calls;
	if (state->setop >= 0)
		for (int side = 0; side < 2; side++)
		{
			const TessProjectionStats *computed =
				tess_projection_stats(state->side_projections[side]);

			values[AGG_COMPUTED] += computed->chain_datums + computed->row_datums;
		}
	else if (state->projection != NULL)
	{
		const TessProjectionStats *computed = tess_projection_stats(state->projection);

		values[AGG_COMPUTED] = computed->chain_datums + computed->row_datums;
	}
	values[AGG_GATHERED] = state->setop >= 0 ?
		tess_input_gathered_rows(state->side_inputs[0]) +
		tess_input_gathered_rows(state->side_inputs[1]) :
		tess_input_gathered_rows(state->input);
	values[AGG_GROUPS] = state->groups;
	values[AGG_MEMORY] = state->peak_memory;
	values[AGG_GROWS] = state->grows;
	values[AGG_PARTITIONS] = state->partitions;
	values[AGG_EVICTIONS] = state->evictions;
	values[AGG_SPILLED] = state->spilled;
	values[AGG_DISK] = state->disk_bytes;
	values[AGG_SPLITS] = state->splits;
	values[AGG_EARLY] = state->early_emits;
	values[AGG_SPILLED_ROWS] = state->spilled_rows;
	values[AGG_SPILLED_PAIRS] = state->spilled_pairs;
}

/* The totals of every participant in a parallel plan, else the node's own. */
static void
agg_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessAggState *state = (TessAggState *) css;
	const uint64 *totals;
	uint64		own[AGG_NCOUNTERS];

	if (state->nkeys > 0)
	{
		CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
		List	   *context = set_deparse_context_plan(es->deparse_cxt,
													   css->ss.ps.plan, ancestors);
		bool		useprefix = es->rtable_size > 1 || es->verbose;
		List	   *keys = NIL;

		foreach_node(TargetEntry, entry, cscan->custom_scan_tlist)
		{
			if (foreach_current_index(entry) < state->nkeys)
				keys = lappend(keys, deparse_expression((Node *) entry->expr,
														context, useprefix,
														false));
		}
		ExplainPropertyList("Group Key", keys, es);
		if (state->setop >= 0)
			ExplainPropertyText("Set Operation",
								state->setop == SETOPCMD_INTERSECT ? "Intersect" :
								state->setop == SETOPCMD_INTERSECT_ALL ? "Intersect All" :
								state->setop == SETOPCMD_EXCEPT ? "Except" : "Except All", es);
	}
	if (state->partial || state->finalize)
		ExplainPropertyText("Partial Mode", state->partial ? "Partial" : "Finalize", es);
	/* The aggregates derived from states kept for several (agg_derive). */
	if (es->verbose && state->nderived > 0)
		ExplainPropertyInteger("Folded Aggregates", NULL, state->nderived, es);
	if (!es->analyze)
		return;
	agg_counters(state, own);
	totals = tess_shared_stats_totals_or(state->stats, own);
	/* As the core's hashed aggregate shows its table and its spill. */
	if (state->nkeys > 0)
	{
		tess_explain_kb("Memory Usage", totals[AGG_MEMORY], es);
		if (totals[AGG_PARTITIONS] > 0)
		{
			ExplainPropertyInteger("Batches", NULL, totals[AGG_PARTITIONS], es);
			tess_explain_kb("Disk Usage", totals[AGG_DISK], es);
		}
	}
	/* The batches, the kernels and the table's work: VERBOSE only. */
	if (!es->verbose)
		return;
	ExplainPropertyInteger("Input Batches", NULL, totals[AGG_BATCHES], es);
	ExplainPropertyInteger("Input Rows", NULL, totals[AGG_ROWS], es);
	if (totals[AGG_GATHERED] > 0)
		ExplainPropertyInteger("Gathered Rows", NULL, totals[AGG_GATHERED], es);
	ExplainPropertyInteger("Kernel Calls", NULL, totals[AGG_CALLS], es);
	if (state->projection != NULL)
		ExplainPropertyInteger("Computed Datums", NULL, totals[AGG_COMPUTED], es);
	if (state->nkeys > 0)
	{
		ExplainPropertyInteger("Groups", NULL, totals[AGG_GROUPS], es);
		ExplainPropertyInteger("Table Grows", NULL, totals[AGG_GROWS], es);
		if (totals[AGG_EARLY] > 0)
			ExplainPropertyInteger("Early Emits", NULL, totals[AGG_EARLY], es);
		if (totals[AGG_PARTITIONS] > 0)
		{
			ExplainPropertyInteger("Evictions", NULL, totals[AGG_EVICTIONS], es);
			ExplainPropertyInteger("Spilled Chunks", NULL, totals[AGG_SPILLED], es);
			if (totals[AGG_SPILLED_ROWS] > 0)
				ExplainPropertyInteger("Spilled Rows", NULL, totals[AGG_SPILLED_ROWS], es);
			if (totals[AGG_SPILLED_PAIRS] > 0)
				ExplainPropertyInteger("Spilled Pairs", NULL, totals[AGG_SPILLED_PAIRS], es);
			if (totals[AGG_SPLITS] > 0)
				ExplainPropertyInteger("Split Partitions", NULL, totals[AGG_SPLITS], es);
		}
	}
}

/*
 * A parallel plan: the node shares only its counters, in the rows of its
 * chunk; the child divides the work and the Finalize Aggregate above the
 * Gather combines the participants' values.
 */
TESS_NODE_STATS_CALLBACKS(agg, TessAggState, AGG_NCOUNTERS, agg_counters)

static const CustomExecMethods agg_exec_methods = {
	.CustomName = "TessAgg",
	.BeginCustomScan = agg_begin,
	.ExecCustomScan = agg_exec,
	.EndCustomScan = agg_end,
	.ReScanCustomScan = agg_rescan,
	.ExplainCustomScan = agg_explain,
	TESS_NODE_STATS_METHODS(agg),
};

static Node *
agg_create_state(CustomScan *cscan)
{
	TessAggState *state = (TessAggState *)
		newNode(sizeof(TessAggState), T_CustomScanState);

	state->css.methods = &agg_exec_methods;
	return (Node *) state;
}

const CustomScanMethods tess_agg_scan_methods = {
	.CustomName = "TessAgg",
	.CreateCustomScanState = agg_create_state,
};

const TessNode tess_agg_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_AGG_NODE_NAME,
};

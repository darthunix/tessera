/*
 * The differences between the cores Tessera builds against: PostgreSQL
 * master and Greengage 7, a fork of PostgreSQL 12. abi.h includes this
 * header, so every module sees it. TESS_GREENGAGE is 1 on Greengage.
 * On PostgreSQL master the header defines nothing else.
 */
#ifndef TESSERA_COMPAT_H
#define TESSERA_COMPAT_H

#include "postgres.h"

#ifdef GP_VERSION_NUM
#define TESS_GREENGAGE 1
#else
#define TESS_GREENGAGE 0
#endif

#if TESS_GREENGAGE

#include "access/stratnum.h"
#include "fmgr.h"
#include "miscadmin.h"
#include "nodes/memnodes.h"
#include "nodes/pathnodes.h"
#include "storage/shmem.h"
#include "nodes/nodes.h"
#include "nodes/pg_list.h"
#include "utils/numeric.h"

/* Of utils/fmgrprotos.h, whose names clash with Tessera's own. */
extern Datum int8_numeric(PG_FUNCTION_ARGS);
extern Datum numeric_in(PG_FUNCTION_ARGS);
extern Datum numeric_mul(PG_FUNCTION_ARGS);

/*
 * A copy whose type drops the original's const, as master's copyObject;
 * untyped, as master's too, where the compiler lacks __typeof_unqual__.
 */
#undef copyObject
#if (defined(__clang__) && __clang_major__ >= 19) || \
	(!defined(__clang__) && defined(__GNUC__) && __GNUC__ >= 14)
#define copyObject(obj) ((__typeof_unqual__(*(obj)) *) copyObjectImpl(obj))
#else
#define copyObject(obj) copyObjectImpl(obj)
#endif

/*
 * Remove a callback registered with MemoryContextRegisterResetCallback
 * (PostgreSQL 18), as master does: unlink it from the context's list.
 */
static inline void
MemoryContextUnregisterResetCallback(MemoryContext context,
									 MemoryContextCallback *cb)
{
	MemoryContextCallback *prev = NULL;

	for (MemoryContextCallback *cur = context->reset_cbs; cur != NULL;
		 prev = cur, cur = cur->next)
	{
		if (cur != cb)
			continue;
		if (prev != NULL)
			prev->next = cur->next;
		else
			context->reset_cbs = cur->next;
		return;
	}
	Assert(false);
}

/* A bigint as a numeric (PostgreSQL 14). */
static inline Numeric
int64_to_numeric(int64 value)
{
	return DatumGetNumeric(DirectFunctionCall1(int8_numeric, Int64GetDatum(value)));
}

/*
 * value / 10^scale as a numeric whose display scale is Max(scale, 0)
 * (PostgreSQL 14): a product by 1e-scale, whose scales add up. The
 * callers pass a decimal's scale, a few dozen at most: this version
 * takes scales within +-NUMERIC_MAX_PRECISION only, a narrower range
 * than master's, and refuses the others.
 * shortcut: parses the factor on every call; cache it if a profile shows it.
 */
static inline Numeric
int64_div_fast_to_numeric(int64 value, int scale)
{
	char		factor[16];

	if (scale < -NUMERIC_MAX_PRECISION || scale > NUMERIC_MAX_DISPLAY_SCALE)
		elog(ERROR, "Tessera cannot scale a numeric by 10^%d", scale);
	snprintf(factor, sizeof(factor), "1e%d", -scale);
	return DatumGetNumeric(DirectFunctionCall2(numeric_mul,
											   NumericGetDatum(int64_to_numeric(value)),
											   DirectFunctionCall3(numeric_in,
																   CStringGetDatum(factor),
																   ObjectIdGetDatum(InvalidOid),
																   Int32GetDatum(-1))));
}

/* PostgreSQL 12's numeric has no infinities. */
#define numeric_is_inf(num) ((void) (num), false)

/*
 * Iteration over a List as PostgreSQL 17 writes it. PostgreSQL 12 keeps
 * a list as linked cells, so the state holds the cell and counts the
 * index for foreach_current_index.
 */
typedef struct TessForEachState
{
	const List *list;
	ListCell   *cell;
	int			index;
} TessForEachState;

/* The state at the list's first cell, the list's expression read once. */
static inline TessForEachState
tess_foreach_start(const List *list)
{
	TessForEachState state = {list, list_head(list), 0};

	return state;
}

#define tess_foreach_internal(type, pointer, var, lst, value) \
	for (type pointer var = 0, pointer var##__outerloop = (type pointer) 1; \
		 var##__outerloop; \
		 var##__outerloop = 0) \
		for (TessForEachState var##__state = tess_foreach_start(lst); \
			 var##__state.cell != NULL && (var = (value), true); \
			 var##__state.cell = lnext(var##__state.cell), var##__state.index++)

#define foreach_ptr(type, var, lst) \
	tess_foreach_internal(type, *, var, lst, (type *) lfirst(var##__state.cell))
#define foreach_node(type, var, lst) \
	tess_foreach_internal(type, *, var, lst, castNode(type, lfirst(var##__state.cell)))
#define foreach_int(var, lst) \
	tess_foreach_internal(int, , var, lst, lfirst_int(var##__state.cell))
#define foreach_oid(var, lst) \
	tess_foreach_internal(Oid, , var, lst, lfirst_oid(var##__state.cell))
#define foreach_current_index(var) (var##__state.index)

/* The first n cells of a list, copied, of the list's type (PostgreSQL 15). */
static inline List *
list_copy_head(const List *list, int n)
{
	List	   *result = NIL;
	ListCell   *cell;

	foreach(cell, list)
	{
		if (n-- <= 0)
			break;
		if (IsA(list, IntList))
			result = lappend_int(result, lfirst_int(cell));
		else if (IsA(list, OidList))
			result = lappend_oid(result, lfirst_oid(cell));
		else
			result = lappend(result, lfirst(cell));
	}
	return result;
}

/* A new list of both lists' cells, neither changed (PostgreSQL 13). */
static inline List *
list_concat_copy(const List *list1, const List *list2)
{
	return list_concat(list_copy(list1), list_copy(list2));
}

/* The list without its last cell (PostgreSQL 13). */
static inline List *
list_delete_last(List *list)
{
	ListCell   *prev = NULL;
	ListCell   *cell;

	if (list == NIL)
		return NIL;
	for (cell = list_head(list); lnext(cell) != NULL; cell = lnext(cell))
		prev = cell;
	/* Frees the cell, and the list with its only one. */
	return list_delete_cell(list, cell, prev);
}

/* Grow an allocation and zero the new tail (PostgreSQL 16). */
static inline void *
repalloc0(void *pointer, Size oldsize, Size size)
{
	char	   *result;

	if (oldsize > size)
		elog(ERROR, "invalid repalloc0 call: oldsize %zu, new size %zu",
			 oldsize, size);
	result = repalloc(pointer, size);
	memset(result + oldsize, 0, size - oldsize);
	return result;
}

#define repalloc0_array(pointer, type, oldcount, count) \
	((type *) repalloc0(pointer, mul_size(sizeof(type), (oldcount)), \
						mul_size(sizeof(type), (count))))

/* Attributes of PostgreSQL 18 and master. */
#define pg_noreturn __attribute__((noreturn))
#define pg_always_inline pg_attribute_always_inline
#define pg_assume(expr) \
	do { \
		if (!(expr)) \
			pg_unreachable(); \
	} while (0)

/* Greengage's magic block names no module. */
#define PG_MODULE_MAGIC_EXT(...) PG_MODULE_MAGIC

/* The extension's prefix: PostgreSQL 12 warns of placeholders only. */
#define MarkGUCPrefixReserved(prefix) EmitWarningsOnPlaceholders(prefix)

/* Attributes the copied lib/sort_template.h uses. */
#define pg_attribute_no_sanitize_function()

/*
 * hash_mem: PostgreSQL 12 has no hash_mem_multiplier, so work_mem.
 * shortcut: Greengage gives a node its own budget, operatorMemKB; take it
 * when the nodes read their memory through the plan state.
 */
static inline size_t
get_hash_memory_limit(void)
{
	return (size_t) work_mem * 1024;
}

/* Names of PostgreSQL 13 to 18. */
#define VARHDRSZ_COMPRESSED offsetof(varattrib_4b, va_compressed.va_data)
#define TYPALIGN_DOUBLE 'd'
typedef Datum bh_node_type;
#define COMPARE_LT BTLessStrategyNumber
#define COMPARE_GT BTGreaterStrategyNumber
#define pk_cmptype pk_strategy
#define get_opfamily_member_for_cmptype(family, left, right, cmp) \
	get_opfamily_member((family), (left), (right), (cmp))
#define TupleDescCompactAttr(desc, i) TupleDescAttr((desc), (i))
/* No incremental sort before PostgreSQL 13: no path has this tag. */
#define T_IncrementalSortPath T_Invalid

/* Flags of the Greengage patches, for headers installed before them. */
#ifndef CUSTOMPATH_SUPPORT_PROJECTION
#define CUSTOMPATH_SUPPORT_PROJECTION 0x0004
#endif
#ifndef CUSTOMPATH_GP_LAST_CHILD_FIRST
#define CUSTOMPATH_GP_LAST_CHILD_FIRST 0x00010000
#endif

/* The 64-bit finalizer of MurmurHash3 (PostgreSQL 13), as master's. */
static inline uint64
murmurhash64(uint64 data)
{
	uint64		h = data;

	h ^= h >> 33;
	h *= UINT64CONST(0xff51afd7ed558ccd);
	h ^= h >> 33;
	h *= UINT64CONST(0xc4ceb9fe1a85ec53);
	h ^= h >> 33;
	return h;
}

/*
 * The member of an equivalence class that is expr, binary relabeling
 * aside (PostgreSQL 13): no constant, and a child only of relids.
 */
static inline EquivalenceMember *
find_ec_member_matching_expr(EquivalenceClass *ec, Expr *expr, Relids relids)
{
	ListCell   *cell;

	while (expr != NULL && IsA(expr, RelabelType))
		expr = ((RelabelType *) expr)->arg;
	foreach(cell, ec->ec_members)
	{
		EquivalenceMember *member = (EquivalenceMember *) lfirst(cell);
		Expr	   *member_expr = member->em_expr;

		if (member->em_is_const ||
			(member->em_is_child && !bms_is_subset(member->em_relids, relids)))
			continue;
		while (member_expr != NULL && IsA(member_expr, RelabelType))
			member_expr = ((RelabelType *) member_expr)->arg;
		if (equal(member_expr, expr))
			return member;
	}
	return NULL;
}

#endif							/* TESS_GREENGAGE */

/*
 * The segments a relation's tuples and pages spread over, by which the
 * planner turns its counts into those of one segment: Greengage's paths
 * count one segment's rows, a distributed table's tuples dividing among
 * its segments and a replicated one's not (adjust_reloptinfo in
 * costsize.c); one on PostgreSQL.
 */
#if TESS_GREENGAGE
#include "catalog/gp_distribution_policy.h"

static inline double
tess_rel_segments(const RelOptInfo *rel)
{
	return rel->cdbpolicy != NULL && rel->cdbpolicy->ptype == POLICYTYPE_PARTITIONED ?
		rel->cdbpolicy->numsegments : 1;
}
#else
#define tess_rel_segments(rel) ((void) (rel), 1.0)
#endif

/*
 * The leading attributes every tuple has present, non-NULL and by value
 * (master's guaranteed prefix); none on Greengage, which keeps no such
 * knowledge.
 */
#if TESS_GREENGAGE
#define tess_slot_first_nonguaranteed(slot) ((void) (slot), 0)
#define tess_desc_first_nonguaranteed(desc) ((void) (desc), 0)
#else
#define tess_slot_first_nonguaranteed(slot) ((slot)->tts_first_nonguaranteed)
#define tess_desc_first_nonguaranteed(desc) ((desc)->firstNonGuaranteedAttr)
#endif

/*
 * The flag that sends a setting to Greengage's segments: a newly started
 * segment process gets only the synchronized ones, and an extension's are
 * not by default.
 */
#if TESS_GREENGAGE
#define TESS_GUC_SYNC GUC_GPDB_NEED_SYNC
#else
#define TESS_GUC_SYNC 0
#endif

/*
 * Calls whose arguments differ between the cores. PostgreSQL 12 costs
 * the aggregates of the clauses it is given, the target list and HAVING
 * as its grouping does; master costs every aggregate of the query. Its
 * grouping clause is the query's, not the processed one of PostgreSQL 16.
 */
#if TESS_GREENGAGE
#define tess_deparse_context(es, ps, ancestors) \
	set_deparse_context_planstate((es)->deparse_cxt, (Node *) (ps), (ancestors))
#define tess_explain_rtable_size(es) list_length((es)->rtable)
#define tess_agg_clause_costs(root, split, costs) \
	do { \
		get_agg_clause_costs((root), (Node *) (root)->processed_tlist, (split), (costs)); \
		get_agg_clause_costs((root), (root)->parse->havingQual, (split), (costs)); \
	} while (0)
#define tess_estimate_num_groups(root, exprs, rows) \
	estimate_num_groups((root), (exprs), (rows), NULL)
#define tess_clause_selectivity(root, clause, relid, jointype, sjinfo) \
	clause_selectivity((root), (clause), (relid), (jointype), (sjinfo), false)
#define tess_group_clause(root) ((root)->parse->groupClause)
#define tess_distinct_clause(root) ((root)->parse->distinctClause)
#define tess_limit_with_ties(parse) ((void) (parse), false)
#else
#define tess_deparse_context(es, ps, ancestors) \
	set_deparse_context_plan((es)->deparse_cxt, (ps)->plan, (ancestors))
#define tess_explain_rtable_size(es) ((es)->rtable_size)
#define tess_agg_clause_costs(root, split, costs) \
	get_agg_clause_costs((root), (split), (costs))
#define tess_estimate_num_groups(root, exprs, rows) \
	estimate_num_groups((root), (exprs), (rows), NULL, NULL)
#define tess_clause_selectivity(root, clause, relid, jointype, sjinfo) \
	clause_selectivity((root), (clause), (relid), (jointype), (sjinfo))
#define tess_group_clause(root) ((root)->processed_groupClause)
#define tess_distinct_clause(root) ((root)->processed_distinctClause)
#define tess_limit_with_ties(parse) ((parse)->limitOption == LIMIT_OPTION_WITH_TIES)
#endif

#endif							/* TESSERA_COMPAT_H */

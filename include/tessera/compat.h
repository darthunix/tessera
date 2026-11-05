/*
 * The core Tessera builds against, told by what it has rather than by
 * its name, so that a new fork of PostgreSQL needs its properties
 * written here and little else. Two numbers describe a core:
 *
 * TESS_PG_MAJOR  the release of PostgreSQL the core is or forks: 12 for
 *                Greengage 7, 20 for master.
 * TESS_MPP       1 for a fork of the Greenplum family: a coordinator
 *                plans for segments, Motions move rows between the
 *                slices of a plan, tables may be append-only, and there
 *                is no PostgreSQL parallel query (TESS_PARALLEL_QUERY 0).
 *
 * Code that follows an API writes TESS_PG_MAJOR >= N, N the release
 * that brought it; code that follows the family, TESS_MPP. A fork's own
 * patches announce themselves by macros of the headers they change,
 * which the code tests where it includes them. The sections below give
 * an older core the names of each later release, a section a release:
 * a fork of an older PostgreSQL gets the names of every release after
 * its own, and a release whose shims are not written stops the build.
 * The headers a release added come from include/compat/pgN (host.mk).
 * Every module includes this header first (host.mk).
 */
#ifndef TESSERA_COMPAT_H
#define TESSERA_COMPAT_H

#include "postgres.h"

#define TESS_PG_MAJOR (PG_VERSION_NUM / 10000)
#ifdef GP_VERSION_NUM
#define TESS_MPP 1
#else
#define TESS_MPP 0
#endif
#define TESS_PARALLEL_QUERY (!TESS_MPP)

/*
 * The cores whose shims are written: master, and a fork of the
 * Greenplum family on PostgreSQL 12.
 */
#if !(TESS_PG_MAJOR >= 20 || (TESS_MPP && TESS_PG_MAJOR == 12))
#error "Tessera builds against PostgreSQL master or a Greenplum fork of PostgreSQL 12"
#endif

#if TESS_PG_MAJOR < 20
#include "access/stratnum.h"
#include "fmgr.h"
#include "miscadmin.h"
#include "nodes/memnodes.h"
#include "nodes/nodes.h"
#include "nodes/pathnodes.h"
#include "nodes/pg_list.h"
#include "port/pg_bitutils.h"
#include "storage/shmem.h"
#include "utils/numeric.h"
#endif

/* ---------------------------------------------- what PostgreSQL 13 added */
#if TESS_PG_MAJOR < 13

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

/* The greatest power of two not above num, num > 0 (PostgreSQL 13). */
static inline uint64
pg_prevpower2_64(uint64 num)
{
	Assert(num > 0);
	return ((uint64) 1) << pg_leftmost_one_pos64(num);
}

/* The least power of two not below num, num > 0 (PostgreSQL 13). */
static inline uint64
pg_nextpower2_64(uint64 num)
{
	Assert(num > 0 && num <= PG_UINT64_MAX / 2 + 1);
	if ((num & (num - 1)) == 0)
		return num;
	return ((uint64) 1) << (pg_leftmost_one_pos64(num) + 1);
}

#define TYPALIGN_DOUBLE 'd'
/* No incremental sort: no path has this tag. */
#define T_IncrementalSortPath T_Invalid

#define tess_deparse_context(es, ps, ancestors) \
	set_deparse_context_planstate((es)->deparse_cxt, (Node *) (ps), (ancestors))
#define tess_limit_with_ties(parse) ((void) (parse), false)
#else
#define tess_deparse_context(es, ps, ancestors) \
	set_deparse_context_plan((es)->deparse_cxt, (ps)->plan, (ancestors))
#define tess_limit_with_ties(parse) ((parse)->limitOption == LIMIT_OPTION_WITH_TIES)
#endif

/* ---------------------------------------------- what PostgreSQL 14 added */
#if TESS_PG_MAJOR < 14

/* Of utils/fmgrprotos.h, whose names clash with Tessera's own. */
extern Datum int8_numeric(PG_FUNCTION_ARGS);
extern Datum numeric_in(PG_FUNCTION_ARGS);
extern Datum numeric_mul(PG_FUNCTION_ARGS);

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

/* Numerics without infinities. */
#define numeric_is_inf(num) ((void) (num), false)

#define VARHDRSZ_COMPRESSED offsetof(varattrib_4b, va_compressed.va_data)

/*
 * The memory of a hash table in bytes (PostgreSQL 14): hash_mem in
 * kilobytes in PostgreSQL 13, work_mem before its hash_mem_multiplier.
 * shortcut: the Greenplum family gives a node its own budget,
 * operatorMemKB; take it when the nodes read their memory through the
 * plan state.
 */
static inline size_t
get_hash_memory_limit(void)
{
#if TESS_PG_MAJOR < 13
	return (size_t) work_mem * 1024;
#else
	return (size_t) get_hash_mem() * 1024;
#endif
}

/*
 * The member of an equivalence class that is expr, binary relabeling
 * aside (PostgreSQL 14): no constant, and a child only of relids.
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

/*
 * The aggregates' costs: an older core costs those of the clauses it is
 * given, the target list and HAVING as its grouping does; master every
 * aggregate of the query.
 */
#define tess_agg_clause_costs(root, split, costs) \
	do { \
		get_agg_clause_costs((root), (Node *) (root)->processed_tlist, (split), (costs)); \
		get_agg_clause_costs((root), (root)->parse->havingQual, (split), (costs)); \
	} while (0)
#define tess_estimate_num_groups(root, exprs, rows) \
	estimate_num_groups((root), (exprs), (rows), NULL)
#else
#define tess_agg_clause_costs(root, split, costs) \
	get_agg_clause_costs((root), (split), (costs))
#define tess_estimate_num_groups(root, exprs, rows) \
	estimate_num_groups((root), (exprs), (rows), NULL, NULL)
#endif

/* ---------------------------------------------- what PostgreSQL 15 added */
#if TESS_PG_MAJOR < 15

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

/* The extension's prefix: an older core warns of placeholders only. */
#define MarkGUCPrefixReserved(prefix) EmitWarningsOnPlaceholders(prefix)

/*
 * The flag of a custom path that projects (PostgreSQL 15), which a fork
 * may carry as a patch; a core without the patch ignores it and projects
 * itself.
 */
#ifndef CUSTOMPATH_SUPPORT_PROJECTION
#define CUSTOMPATH_SUPPORT_PROJECTION 0x0004
#endif
#endif

/* ---------------------------------------------- what PostgreSQL 16 added */
#if TESS_PG_MAJOR < 16

/* No right anti join (PostgreSQL 16): no join has this kind. */
#define JOIN_RIGHT_ANTI ((JoinType) -1)

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

/* The grouping clauses as the query wrote them, not processed ones. */
#define tess_group_clause(root) ((root)->parse->groupClause)
#define tess_distinct_clause(root) ((root)->parse->distinctClause)
#else
#define tess_group_clause(root) ((root)->processed_groupClause)
#define tess_distinct_clause(root) ((root)->processed_distinctClause)
#endif

/* ---------------------------------------------- what PostgreSQL 17 added */
#if TESS_PG_MAJOR < 17

#include "port/atomics.h"

/*
 * Advance an atomic to target at least, and give the latest value seen,
 * with a full barrier even when it is left (PostgreSQL 17).
 */
static inline uint64
pg_atomic_monotonic_advance_u64(volatile pg_atomic_uint64 *ptr, uint64 target)
{
	uint64		currval = pg_atomic_read_u64(ptr);

	if (currval >= target)
	{
		pg_memory_barrier();
		return currval;
	}
	while (currval < target)
	{
		if (pg_atomic_compare_exchange_u64(ptr, &currval, target))
			return target;
	}
	return currval;
}

#if TESS_PG_MAJOR >= 13
#error "foreach_ptr over the lists of PostgreSQL 13 to 16 is not written"
#endif
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

typedef Datum bh_node_type;

/* The 64-bit finalizer of MurmurHash3 (PostgreSQL 17), as master's. */
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
#endif

/* ---------------------------------------------- what PostgreSQL 18 added */
#if TESS_PG_MAJOR < 18

#define pg_noreturn __attribute__((noreturn))
#define COMPARE_LT BTLessStrategyNumber
#define COMPARE_GT BTGreaterStrategyNumber
#define pk_cmptype pk_strategy
#define get_opfamily_member_for_cmptype(family, left, right, cmp) \
	get_opfamily_member((family), (left), (right), (cmp))
#define TupleDescCompactAttr(desc, i) TupleDescAttr((desc), (i))
/* An older magic block names no module. */
#define PG_MODULE_MAGIC_EXT(...) PG_MODULE_MAGIC
#define tess_explain_rtable_size(es) list_length((es)->rtable)
#else
#define tess_explain_rtable_size(es) ((es)->rtable_size)
#endif

/* ---------------------------------------------- what PostgreSQL 19 added */
#if TESS_PG_MAJOR < 19

/*
 * Remove a callback registered with MemoryContextRegisterResetCallback
 * (PostgreSQL 19), as master does: unlink it from the context's list.
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

#define pg_always_inline pg_attribute_always_inline
#define pg_assume(expr) \
	do { \
		if (!(expr)) \
			pg_unreachable(); \
	} while (0)

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

/* No guaranteed prefix of attributes present, not NULL and by value. */
#define tess_slot_first_nonguaranteed(slot) ((void) (slot), 0)
#define tess_desc_first_nonguaranteed(desc) ((void) (desc), 0)
#else
#define tess_slot_first_nonguaranteed(slot) ((slot)->tts_first_nonguaranteed)
#define tess_desc_first_nonguaranteed(desc) ((desc)->firstNonGuaranteedAttr)
#endif

/* ----------------------------------------------- what master (20) added */
#if TESS_PG_MAJOR < 20

/* An attribute the copied lib/sort_template.h uses. */
#define pg_attribute_no_sanitize_function()
#endif

/* ---------------------------------------- the forks of the Greenplum family */
#if TESS_MPP

/*
 * The patches a fork needs for custom scans to have children of their
 * own: walkers, mutators, squelch and the slice table visit custom_plans
 * and custom_ps. Without them a plan under a Tessera node loses its
 * Motions.
 */
#include "nodes/extensible.h"
#ifndef CUSTOMSCAN_GP_CHILDREN
#error "this fork lacks the custom scan patches Tessera needs"
#endif

/*
 * The segments a relation's tuples and pages spread over, by which the
 * planner turns its counts into those of one segment: the family's paths
 * count one segment's rows, a distributed table's tuples dividing among
 * its segments and a replicated one's not (adjust_reloptinfo in
 * costsize.c).
 */
#include "catalog/gp_distribution_policy.h"

static inline double
tess_rel_segments(const RelOptInfo *rel)
{
	return rel->cdbpolicy != NULL && rel->cdbpolicy->ptype == POLICYTYPE_PARTITIONED ?
		rel->cdbpolicy->numsegments : 1;
}

/*
 * The flag that sends a setting to the segments: a newly started segment
 * process gets only the synchronized ones, and an extension's are not by
 * default.
 */
#define TESS_GUC_SYNC GUC_GPDB_NEED_SYNC

/* The family's clause_selectivity takes whether to damp the estimate. */
#define tess_clause_selectivity(root, clause, relid, jointype, sjinfo) \
	clause_selectivity((root), (clause), (relid), (jointype), (sjinfo), false)
#else
#define tess_rel_segments(rel) ((void) (rel), 1.0)
#define TESS_GUC_SYNC 0
#define tess_clause_selectivity(root, clause, relid, jointype, sjinfo) \
	clause_selectivity((root), (clause), (relid), (jointype), (sjinfo))
#endif

#endif							/* TESSERA_COMPAT_H */

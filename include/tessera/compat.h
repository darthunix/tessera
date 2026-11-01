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

#include "fmgr.h"
#include "nodes/memnodes.h"
#include "nodes/nodes.h"
#include "nodes/pg_list.h"
#include "utils/numeric.h"

/* Of utils/fmgrprotos.h, whose names clash with Tessera's own. */
extern Datum int8_numeric(PG_FUNCTION_ARGS);
extern Datum numeric_in(PG_FUNCTION_ARGS);
extern Datum numeric_mul(PG_FUNCTION_ARGS);

/* A copy whose type drops the original's const, as master's copyObject. */
#undef copyObject
#define copyObject(obj) ((__typeof_unqual__(*(obj)) *) copyObjectImpl(obj))

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
 * (PostgreSQL 14): a product by 1e-scale, whose scales add up.
 * shortcut: parses the factor on every call; cache it if a profile shows it.
 */
static inline Numeric
int64_div_fast_to_numeric(int64 value, int scale)
{
	char		factor[16];

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

#define tess_foreach_internal(type, pointer, var, lst, value) \
	for (type pointer var = 0, pointer var##__outerloop = (type pointer) 1; \
		 var##__outerloop; \
		 var##__outerloop = 0) \
		for (TessForEachState var##__state = {(lst), list_head(lst), 0}; \
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

/* The first n cells of a list, copied (PostgreSQL 15). */
static inline List *
list_copy_head(const List *list, int n)
{
	List	   *result = NIL;
	ListCell   *cell;

	foreach(cell, list)
	{
		if (n-- <= 0)
			break;
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
	return list_length(list) <= 1 ? NIL : list_truncate(list, list_length(list) - 1);
}

/* Grow an allocation and zero the new tail (PostgreSQL 16). */
static inline void *
repalloc0(void *pointer, Size oldsize, Size size)
{
	char	   *result = repalloc(pointer, size);

	if (size > oldsize)
		memset(result + oldsize, 0, size - oldsize);
	return result;
}

#define repalloc0_array(pointer, type, oldcount, count) \
	((type *) repalloc0(pointer, sizeof(type) * (oldcount), sizeof(type) * (count)))

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

#endif							/* TESS_GREENGAGE */

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

#endif							/* TESSERA_COMPAT_H */

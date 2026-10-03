/* Definitions shared within the Tessera nodes module. */
#ifndef TESSERA_NODES_INTERNAL_H
#define TESSERA_NODES_INTERNAL_H

#include "catalog/pg_type_d.h"
#include "utils/sortsupport.h"
#include "utils/typcache.h"

#include "tessera/kernel_ops.h"
#include "tessera/node.h"
#include "tessera/planner.h"
#include "tessera/sort.h"

#include "abbrev.h"
#include "costs.h"
#include "prune.h"
#include "registry.h"

/*
 * The table's kind of a key of this type: a value its word holds whole and
 * compares bit for bit, sign-extended to eight bytes as the kernels hash,
 * order and give it back, as PostgreSQL's Datum of the type is: int2,
 * int4, date and bool as INT4, int8, timestamp and timestamptz as INT8.
 * False for any other type: its values go by reference, or compare other
 * than bit for bit, or come back unlike the core's Datum (oid).
 */
static inline bool
tess_word_key_kind(Oid type, TessTableKeyKind *kind)
{
	switch (type)
	{
		case INT2OID:
		case INT4OID:
		case DATEOID:
		case BOOLOID:
			*kind = TESS_TABLE_KEY_INT4;
			return true;
		case INT8OID:
		case TIMESTAMPOID:
		case TIMESTAMPTZOID:
			*kind = TESS_TABLE_KEY_INT8;
			return true;
		default:
			return false;
	}
}

/*
 * Whether a sort key's B-tree family orders as the key's word does: the
 * default family of a type a word holds (integer_ops, datetime_ops,
 * bool_ops), its ordinary order, which the word's keeps.
 */
static inline bool
tess_word_key_order(Oid type, Oid opfamily)
{
	TessTableKeyKind kind;

	return tess_word_key_kind(type, &kind) &&
		opfamily == lookup_type_cache(type, TYPECACHE_BTREE_OPFAMILY)->btree_opf;
}

/* Clauses (RestrictInfos) in the order the planner evaluates a plan's quals. */
extern List *tess_order_clauses(PlannerInfo *root, List *rinfos);
extern double tess_parallel_divisor(const Path *path);

/*
 * The sizes of a context of a spill's chunks: the small sizes, so that a
 * chunk past a few kB gets a block of its own, of its size, not a share of
 * a block twice as large, and memory is what the chunks take. Its minimum
 * is its first block, as the small sizes make it, not 0: a context of the
 * small sizes themselves may be one the allocator recycles (aset.c), which
 * keeps the largest chunk a block shares from its first life, 8 kB for a
 * plan's, and then rounded the chunks up to powers of two: a partition of
 * 656 kB read back took 1.3 MB.
 */
#define TESS_CHUNK_CONTEXT_SIZES \
	ALLOCSET_SMALL_INITSIZE, ALLOCSET_SMALL_INITSIZE, ALLOCSET_SMALL_MAXSIZE

/*
 * The weights a grouping's and a join's spills give the one rule that
 * sends partitions to disk (docs/spill.md, "Weights"), set in module.c.
 */
extern double tess_join_spill_start;
extern double tess_join_spill_target;
extern double tess_join_spill_spilled_weight;
extern double tess_join_spill_tail_weight;
extern double tess_join_spill_resident_share;
extern int	tess_join_shared_spill_evictions;

#endif							/* TESSERA_NODES_INTERNAL_H */

/*
 * Sort keys as TessSort and TessGatherMerge take them, and the abbreviated
 * keys of other types (nodes/sort.c, "Other types" in docs/nodes.md).
 */
#ifndef TESSERA_NODES_ABBREV_H
#define TESSERA_NODES_ABBREV_H

#include "nodes/pathnodes.h"
#include "utils/sortsupport.h"

#include "tessera/sort.h"

/* A path key the sort kernels order by: its target's place in target, kind and flags. */
extern bool tess_sort_key_of(PathKey *pathkey, PathTarget *target, Relids relids,
							 int *place, TessSortKey *key);

/*
 * Keys of other types (nodes/sort.c, "Other types" in docs/nodes.md): the
 * planned kind of such a key, whose word is its abbreviated key as an int8;
 * a path key's place, ordering operator and collation; whether its type
 * has an abbreviated key the node takes; sort support for its comparison;
 * and its abbreviated key made a word in its order.
 */
#define TESS_SORT_KIND_GENERIC (-1)

typedef struct TessSortAbbrev
{
	SortSupportData ssup;
	/* How the abbreviated keys order (nodes/sort.c), none without them. */
	int			order;
} TessSortAbbrev;

extern bool tess_sort_generic_key(PathKey *pathkey, PathTarget *target, Relids relids,
								  int *place, Oid *sortop, Oid *collation);

/* Path keys as TessSort and TessGatherMerge take them: lists of int, one entry a key. */
typedef struct TessSortKeys
{
	List	   *places;
	List	   *kinds;
	List	   *flags;
	List	   *sortops;
	List	   *collations;
} TessSortKeys;

extern bool tess_sort_keys(List *pathkeys, PathTarget *target, Relids relids,
						   TessSortKeys *keys);
extern void tess_sort_support(SortSupport ssup, Oid sortop, Oid collation, bool nulls_first);
extern void tess_sort_abbrev_init(TessSortAbbrev *abbrev, Oid sortop, Oid collation,
								  bool nulls_first, Oid type);
extern bool tess_sort_abbreviates(const TessSortAbbrev *abbrev);
extern int64 tess_sort_abbrev_word(TessSortAbbrev *abbrev, Datum value);

#endif							/* TESSERA_NODES_ABBREV_H */

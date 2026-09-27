/*
 * C entry points of the sort of a table's records (crates/tessera-kernels,
 * module sort).
 *
 * A node that sorts keeps its rows as records of the table format
 * (TessRows in tessera/runtime.h, or any table whose keys are the sort
 * keys, in key order) and orders them in two calls: tess_sort_items writes
 * an item of every record of every chunk, linked or not, into words the
 * caller allocates, and tess_sort sorts the items and writes the records'
 * references in order. An item is tess_sort_item_words words: each key's
 * value as bits whose unsigned order is the key's order, after a bit for
 * NULL when the key may be NULL, and the record's reference in the last
 * word's low 32 bits, so that equal keys stay distinct. The library
 * allocates nothing and keeps nothing between calls. See docs/table.md,
 * "Sorting records".
 */
#ifndef TESSERA_SORT_H
#define TESSERA_SORT_H

#include "postgres.h"

#include "tessera/status.h"
#include "tessera/table.h"

/* How a key orders, as bits of TessSortKey.flags. */
#define TESS_SORT_DESCENDING 0x1
#define TESS_SORT_NULLS_FIRST 0x2
/* A row may hold NULL in the key; a NULL in a key without it is an error. */
#define TESS_SORT_NULLABLE 0x4

/* The most words an item takes: sixteen int8 keys that may be NULL. */
#define TESS_SORT_MAX_ITEM_WORDS 17

/* One sort key: the table's key of the same place, and how it orders. */
typedef struct TessSortKey
{
	TessTableKeyKind kind;
	uint32		flags;
} TessSortKey;

/* Sizes and offsets the Rust side was built with, for layout checks. */
typedef enum TessSortLayoutKind
{
	TESS_SORT_LAYOUT_KEY_SIZE = 0,
	TESS_SORT_LAYOUT_KEY_FLAGS_OFFSET = 1
} TessSortLayoutKind;

/* The size or offset for a kind, or 0 for an unknown one. */
extern Size tess_sort_layout(TessSortLayoutKind kind);

/* The words of one item for nkeys keys (1 to TESS_TABLE_MAX_KEYS). */
extern TessStatusCode tess_sort_item_words(int nkeys,
										   const TessSortKey *keys,
										   int *words,
										   TessStatus *status);

/*
 * Write the item of every record of the table's chunks, in the order
 * appended, into items (nwords words, which must hold them all) and their
 * count into *count. keys are the table's keys, one each, of its kinds;
 * the chunks must not change meanwhile.
 */
extern TessStatusCode tess_sort_items(const TessTableRef *table,
									  int nkeys,
									  const TessSortKey *keys,
									  uint64 *items,
									  Size nwords,
									  uint64 *count,
									  TessStatus *status);

/*
 * Sort nitems items of words words each at items, and write the reference
 * of each, in order, into refs (nitems slots).
 */
extern TessStatusCode tess_sort(uint64 *items,
								Size nitems,
								int words,
								uint32 *refs,
								TessStatus *status);

#endif							/* TESSERA_SORT_H */

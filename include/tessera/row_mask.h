/* Mutable bitmap identifying active physical rows in one batch. */
#ifndef TESSERA_ROW_MASK_H
#define TESSERA_ROW_MASK_H

#include "postgres.h"

#include "port/pg_bitutils.h"

/*
 * A fixed-layout view of the rows that remain active in one batch.
 *
 * The mask does not own bits. The array has tess_row_mask_word_count(nrows)
 * elements, and bits beyond nrows must be clear. Only an exclusive owner may
 * modify it.
 */
typedef struct TessRowMask
{
	/* Number of physical rows represented by the bitmap. */
	int			nrows;
	/* Borrowed array with one bit per row, stored in 64-bit chunks. */
	uint64	   *bits;
} TessRowMask;

/* Return the number of uint64 elements needed for nrows bits. */
static inline int
tess_row_mask_word_count(int nrows)
{
	Assert(nrows >= 0);
	return ((uint32) nrows >> 6) + (((uint32) nrows & 63) != 0);
}

/* Return the number of active rows. */
static inline int
tess_row_mask_count(const TessRowMask *mask)
{
	int			nwords;
	int			result = 0;
	int			word;

	Assert(mask != NULL);
	nwords = tess_row_mask_word_count(mask->nrows);
	if (nwords == 0)
		return 0;
	Assert(mask->bits != NULL);
	if (likely(nwords == 1))
		return pg_popcount64(mask->bits[0]);
	for (word = 0; word < nwords; word++)
		result += pg_popcount64(mask->bits[word]);
	return result;
}

/* Return true when row remains active. */
static inline bool
tess_row_mask_contains(const TessRowMask *mask, int row)
{
	Assert(mask != NULL && mask->bits != NULL);
	Assert(row >= 0 && row < mask->nrows);
	return (mask->bits[row / 64] &
			(UINT64CONST(1) << (row % 64))) != 0;
}

/* Remove row from the active set. */
static inline void
tess_row_mask_clear(TessRowMask *mask, int row)
{
	Assert(mask != NULL && mask->bits != NULL);
	Assert(row >= 0 && row < mask->nrows);
	mask->bits[row / 64] &= ~(UINT64CONST(1) << (row % 64));
}

/* Remove rows not present in other; cleared rows can never be restored. */
static inline void
tess_row_mask_intersect(TessRowMask *mask, const TessRowMask *other)
{
	int			nwords;
	int			word;

	Assert(mask != NULL && other != NULL);
	Assert(mask->nrows == other->nrows);
	nwords = tess_row_mask_word_count(mask->nrows);
	Assert(nwords == 0 || (mask->bits != NULL && other->bits != NULL));
	for (word = 0; word < nwords; word++)
		mask->bits[word] &= other->bits[word];
}

/* Pass -1 to start; return -1 after the last selected row. */
static inline int
tess_row_mask_next(const TessRowMask *mask, int previous)
{
	int			nwords;
	int			row;
	int			word;
	uint64		bits;

	Assert(mask != NULL);
	Assert(previous >= -1 && previous < mask->nrows);
	row = previous + 1;
	if (row >= mask->nrows)
		return -1;
	Assert(mask->bits != NULL);
	nwords = tess_row_mask_word_count(mask->nrows);
	word = row / 64;
	bits = mask->bits[word] & (UINT64_MAX << (row % 64));
	for (;;)
	{
		if (bits != 0)
		{
			int			result;

			result = word * 64 + pg_rightmost_one_pos64(bits);
			return result < mask->nrows ? result : -1;
		}
		if (++word >= nwords)
			return -1;
		bits = mask->bits[word];
	}
}

#endif /* TESSERA_ROW_MASK_H */

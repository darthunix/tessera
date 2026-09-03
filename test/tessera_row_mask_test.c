#include "postgres.h"

#include "fmgr.h"

#include "tessera/row_mask.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_row_mask);

static bool
rows_equal(const TessRowMask *mask, const int *expected, int nexpected)
{
	int			found = 0;
	int			row = -1;

	while ((row = tess_row_mask_next(mask, row)) >= 0)
	{
		if (found >= nexpected || row != expected[found])
			return false;
		found++;
	}
	return found == nexpected;
}

Datum
tessera_test_row_mask(PG_FUNCTION_ARGS)
{
	TessRowMask zero = {0, NULL};
	uint64		empty_bits = 0;
	TessRowMask empty = {7, &empty_bits};
	uint64		small_bits = (UINT64CONST(1) << 0) |
		(UINT64CONST(1) << 2) | (UINT64CONST(1) << 5);
	TessRowMask small = {6, &small_bits};
	const int	small_rows[] = {0, 2, 5};
	const int	small_after_clear[] = {0, 5};
	uint64		wide_bits[2] = {
		(UINT64CONST(1) << 0) | (UINT64CONST(1) << 63),
		(UINT64CONST(1) << 0) | (UINT64CONST(1) << 5),
	};
	TessRowMask wide = {70, wide_bits};
	const int	wide_rows[] = {0, 63, 64, 69};
	uint64		keep_bits[2] = {
		(UINT64CONST(1) << 1) | (UINT64CONST(1) << 63),
		(UINT64CONST(1) << 0) | (UINT64CONST(1) << 5),
	};
	const TessRowMask keep = {70, keep_bits};
	const int	wide_after_intersect[] = {63, 69};

	if (tess_row_mask_word_count(0) != 0 ||
		tess_row_mask_word_count(1) != 1 ||
		tess_row_mask_word_count(64) != 1 ||
		tess_row_mask_word_count(65) != 2 ||
		tess_row_mask_count(&zero) != 0 ||
		tess_row_mask_next(&zero, -1) != -1 ||
		tess_row_mask_count(&empty) != 0 ||
		tess_row_mask_next(&empty, -1) != -1)
		PG_RETURN_BOOL(false);

	if (tess_row_mask_count(&small) != 3 ||
		!tess_row_mask_contains(&small, 2) ||
		!rows_equal(&small, small_rows, lengthof(small_rows)))
		PG_RETURN_BOOL(false);
	tess_row_mask_clear(&small, 2);
	if (tess_row_mask_contains(&small, 2) ||
		!rows_equal(&small, small_after_clear,
			lengthof(small_after_clear)))
		PG_RETURN_BOOL(false);

	if (tess_row_mask_count(&wide) != 4 ||
		!rows_equal(&wide, wide_rows, lengthof(wide_rows)))
		PG_RETURN_BOOL(false);
	tess_row_mask_clear(&wide, 64);
	tess_row_mask_intersect(&wide, &keep);
	PG_RETURN_BOOL(tess_row_mask_count(&wide) == 2 &&
		!tess_row_mask_contains(&wide, 1) &&
		!tess_row_mask_contains(&wide, 64) &&
		rows_equal(&wide, wide_after_intersect,
			lengthof(wide_after_intersect)));
}

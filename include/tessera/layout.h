/* Logical column layout shared by independent Tessera modules. */
#ifndef TESSERA_LAYOUT_H
#define TESSERA_LAYOUT_H

#include "postgres.h"

#include "tessera/abi.h"

/*
 * Mapping from PostgreSQL plan targets to compact batch columns.
 *
 * target_columns has ntargets entries. A NULL map means target N uses column
 * N. Otherwise, -1 means that a target has no representation in the batch.
 */
typedef struct TessLayout
{
	Size		struct_size;
	/* Number of logical columns available in the batch. */
	int			ncolumns;
	/* Number of plan target-list positions described by the map. */
	int			ntargets;
	/* Borrowed target-to-column map, or NULL for an identity map. */
	const int  *target_columns;
} TessLayout;

#define TESS_LAYOUT_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessLayout, target_columns)

/* Return the batch column for a zero-based plan target. */
static inline int
tess_layout_column(const TessLayout *layout, int target)
{
	Assert(layout != NULL);
	Assert(target >= 0 && target < layout->ntargets);
	return layout->target_columns == NULL ? target :
		layout->target_columns[target];
}

#endif /* TESSERA_LAYOUT_H */

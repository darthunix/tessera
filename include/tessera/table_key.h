/*
 * The keys of the kernels' table: their kinds and a batch's key column,
 * for a header that needs them without the table's calls (tessera/node.h,
 * tessera/runtime_rows.h). Part of tessera/table.h.
 */
#ifndef TESSERA_TABLE_KEY_H
#define TESSERA_TABLE_KEY_H

#include "postgres.h"

#include "tessera/batch.h"
#include "tessera/row_mask.h"

/* The most keys a record holds. */
#define TESS_TABLE_MAX_KEYS 16

/* What a key column holds; every key takes an 8-byte slot in a record. */
typedef enum TessTableKeyKind
{
	/* An int4 Datum, sign-extended into its slot. */
	TESS_TABLE_KEY_INT4 = 1,
	/* An int8 Datum. */
	TESS_TABLE_KEY_INT8 = 2
} TessTableKeyKind;

/*
 * One key of a batch: a Datum column of the kind, with prepared as its
 * readiness (NULL when the whole column is initialized), as for the
 * kernels. Every key column has the batch's row count.
 */
typedef struct TessTableKey
{
	TessTableKeyKind kind;
	const TessDatumColumn *column;
	const TessRowMask *prepared;
} TessTableKey;

#endif							/* TESSERA_TABLE_KEY_H */

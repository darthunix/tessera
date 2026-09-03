/* Format-neutral batch contract shared by independent Tessera modules. */
#ifndef TESSERA_BATCH_H
#define TESSERA_BATCH_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/row_mask.h"

#define TESS_BATCH_ABI_VERSION 0
#define TESS_BATCH_OPS_ABI_VERSION 0

/* Why a column is being converted to PostgreSQL Datum values. */
typedef enum TessColumnPurpose
{
	/* Values are needed while the active row mask is being filtered. */
	TESS_COLUMN_FOR_FILTER,
	/* Values are needed after filtering for the node's output. */
	TESS_COLUMN_FOR_PROJECTION
} TessColumnPurpose;

typedef struct TessBatch TessBatch;

/*
 * Borrowed Datum arrays indexed by physical row number.
 *
 * This result structure is governed by the TessBatchOps ABI. A size header is
 * enough to append fields without giving the result a separate ABI version.
 * The caller initializes struct_size. The provider preserves it and fills the
 * remaining fields. isnull has one bool per row; it is not a bitmap.
 */
typedef struct TessDatumColumn
{
	Size		struct_size;
	const Datum *values;
	const bool *isnull;
	int			nrows;
} TessDatumColumn;

#define TESS_DATUM_COLUMN_MIN_SIZE \
	TESS_ABI_SIZE_THROUGH(TessDatumColumn, nrows)

/*
 * Operations supplied by the owner of a batch's physical representation.
 * The table remains valid for at least as long as every batch that uses it.
 */
typedef struct TessBatchOps
{
	uint32		abi_version;
	Size		struct_size;
	/* Required fallback implemented by every physical representation. */
	void		(*get_datum_column) (TessBatch *batch, int column,
								 const TessRowMask *rows,
								 TessColumnPurpose purpose,
								 TessDatumColumn *result);
	/* Optional cleanup called when the exclusive consumer is finished. */
	void		(*release) (TessBatch *batch);
} TessBatchOps;

#define TESS_BATCH_OPS_MIN_SIZE \
	TESS_ABI_SIZE_THROUGH(TessBatchOps, get_datum_column)

/*
 * Format-neutral envelope for one active batch.
 *
 * The producer owns this structure and all physical column storage. An active
 * consumer may only remove rows, request columns, or transfer exclusive use
 * to its parent. table_oid is InvalidOid when rows have no single table of
 * origin.
 */
struct TessBatch
{
	uint32		abi_version;
	Size		struct_size;
	TessRowMask rows;
	Oid			table_oid;
	const TessBatchOps *ops;
	void	   *private_data;
};

#define TESS_BATCH_MIN_SIZE \
	TESS_ABI_SIZE_THROUGH(TessBatch, private_data)

#endif /* TESSERA_BATCH_H */

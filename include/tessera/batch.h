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
 * remaining fields. isnull has one bool per row; it is not a bitmap. A
 * requested NULL row holds an initialized Datum of no meaning (PostgreSQL
 * slots store 0); readers may load it and must not interpret it. Unrequested
 * rows hold values of no meaning too: Tessera's providers allocate their
 * arrays zeroed and leave stale values behind, so every row is initialized
 * memory and a consumer passes the kernels no readiness mask; a provider
 * that leaves rows uninitialized must say so before a consumer may rely on
 * it (no such provider exists yet).
 */
typedef struct TessDatumColumn
{
	Size		struct_size;
	const Datum *values;
	const bool *isnull;
	int			nrows;
	/*
	 * A numeric column's decimals, which a consumer that reads them asks for
	 * by setting accept_decimals: the provider may then answer with
	 * decimal_rows, the rows whose Datum holds not a numeric but the int64
	 * value * 10^decimal_scale, the value's display scale, of at most 18
	 * digits; the other rows hold numerics. A provider that does not know
	 * decimals, or a consumer that did not ask, leaves decimal_rows NULL.
	 */
	bool		accept_decimals;
	const uint64 *decimal_rows;
	int			decimal_scale;
} TessDatumColumn;

#define TESS_DATUM_COLUMN_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessDatumColumn, nrows)
#define TESS_DATUM_COLUMN_DECIMALS_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessDatumColumn, decimal_scale)

/* The decimal rows of a column, or NULL: a column of the size that has them. */
static inline const uint64 *
tess_column_decimal_rows(const TessDatumColumn *column)
{
	return column->struct_size >= TESS_DATUM_COLUMN_DECIMALS_SIZE ?
		column->decimal_rows : NULL;
}

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
	TESS_ABI_SIZE_INCLUDING_FIELD(TessBatchOps, get_datum_column)

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
	TESS_ABI_SIZE_INCLUDING_FIELD(TessBatch, private_data)

#endif /* TESSERA_BATCH_H */

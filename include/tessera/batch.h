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
 * rows hold values of no meaning too, but every row below nrows is written
 * memory: Tessera's providers write each row they add (the builder does not
 * zero its arrays, it stores a value and a flag for every row up to nrows)
 * and leave stale values behind in rows a selection drops, so a consumer
 * passes the kernels no readiness mask; a provider that leaves rows below
 * nrows unwritten must say so before a consumer may rely on it (no such
 * provider exists yet).
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
 * A string column's bytes as the producer holds them, the bytes form of a
 * column of text, varchar or bytea, indexed by physical row number like a
 * TessDatumColumn and governed by the TessBatchOps ABI as it is. For a row
 * asked that is not NULL, data points to the value's bytes and lengths
 * gives their count: the bytes of the value without its header, never
 * compressed nor stored apart; the pointer of such a row is never NULL, an
 * empty value's too. Where the producer holds the column through a
 * dictionary, it may give indexes, each such row the index of its value,
 * 0 or more and below nvalues, the dictionary's count of values, and
 * dictionary the dictionary's identity, a number other than 0 taken from
 * the identity operation of Tessera's API (tessera/bridge.h), one counter
 * of the backend; a producer of a backend whose API lacks that operation
 * gives no dictionary. Batches that follow one another over one dictionary
 * give it the same identity, a dictionary given again after another may
 * get a new one, and no two dictionaries share one, so a consumer may keep
 * what it learned of an index while the identity stays; an identity never
 * passes from one process to another. Rows of one index hold the same
 * bytes; rows of two indexes may too. Without a dictionary, indexes is
 * NULL and nvalues and dictionary 0.
 *
 * Every pointer stays valid, its bytes unchanged, until the batch is
 * released, whatever the consumer asks of the batch in between; the
 * consumer keeps none of them past the release. The bytes passed the
 * column's rule, as a Datum made for the row would. The caller sets
 * struct_size; the provider keeps it, fills the fields struct_size covers
 * and no other, and answers false for a size below
 * TESS_BYTES_COLUMN_MIN_SIZE. Rows not asked hold entries of no meaning
 * that a consumer may load.
 */
typedef struct TessBytesColumn
{
	Size		struct_size;
	int			nrows;
	uint32		nvalues;
	const bool *isnull;
	const char *const *data;
	const uint32 *lengths;
	const int32 *indexes;
	uint64		dictionary;
} TessBytesColumn;

#define TESS_BYTES_COLUMN_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessBytesColumn, dictionary)

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
	/*
	 * Optional (check TESS_ABI_HAS_FIELD, or ask tess_batch_bytes_column): a
	 * string column in the bytes form for the rows of the mask, true when it
	 * is given; false, the result of no meaning, for a column not offered
	 * so, which the consumer then reads through get_datum_column. A row
	 * asked whose bytes fail the column's rule raises the error that making
	 * its Datum would; a row not asked never fails the call.
	 */
	bool		(*get_bytes_column) (TessBatch *batch, int column,
									 const TessRowMask *rows,
									 TessBytesColumn *result);
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
 *
 * values_stored is true when reading a column of the batch evaluates no
 * expression: the values are stored, or deformed from stored tuples, so a
 * read can fail only on a damaged value and has no side effect. A batch
 * that computes a column on request, as a projection's does, leaves it
 * false. The producer sets it on each batch it publishes; a reader asks
 * tess_batch_values_stored, which takes a batch of a size without the
 * field for false.
 */
struct TessBatch
{
	uint32		abi_version;
	Size		struct_size;
	TessRowMask rows;
	Oid			table_oid;
	const TessBatchOps *ops;
	void	   *private_data;
	bool		values_stored;
};

#define TESS_BATCH_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessBatch, private_data)

/*
 * A column of the batch in the bytes form, for the rows of the mask: false
 * for a batch whose operations have no get_bytes_column, and for a column
 * the provider does not offer so; result is initialized here.
 */
static inline bool
tess_batch_bytes_column(TessBatch *batch, int column, const TessRowMask *rows,
						TessBytesColumn *result)
{
	const TessBatchOps *ops = batch->ops;

	*result = (TessBytesColumn) TESS_STRUCT_INITIALIZER(TessBytesColumn);
	if (!TESS_ABI_HAS_FIELD(ops, TessBatchOps, get_bytes_column) ||
		ops->get_bytes_column == NULL)
		return false;
	return ops->get_bytes_column(batch, column, rows, result);
}

/* Whether reading the batch's columns evaluates no expression. */
static inline bool
tess_batch_values_stored(const TessBatch *batch)
{
	return TESS_ABI_HAS_FIELD(batch, TessBatch, values_stored) && batch->values_stored;
}

#endif /* TESSERA_BATCH_H */

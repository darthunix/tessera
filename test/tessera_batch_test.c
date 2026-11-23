#include "postgres.h"

#include "fmgr.h"

#include "tessera/batch.h"
#include "tessera/bridge.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_batch);
PG_FUNCTION_INFO_V1(tessera_test_batch_bytes);
PG_FUNCTION_INFO_V1(tessera_test_batch_dictionary_identity);

#define TEST_NROWS 70
#define TEST_NCOLUMNS 2

typedef struct TestBatchData
{
	Datum		values[TEST_NCOLUMNS][TEST_NROWS];
	bool		isnull[TEST_NCOLUMNS][TEST_NROWS];
	uint64		materialized[TEST_NCOLUMNS][2];
	int			purpose_calls[2];
	int			release_calls;
} TestBatchData;

static void
test_get_datum_column(TessBatch *batch, int column,
					  const TessRowMask *rows, TessColumnPurpose purpose,
					  TessDatumColumn *result)
{
	TestBatchData *data = batch->private_data;
	int			row = -1;

	if (column < 0 || column >= TEST_NCOLUMNS ||
		rows == NULL || rows->nrows != batch->rows.nrows ||
		purpose < TESS_COLUMN_FOR_FILTER ||
		purpose > TESS_COLUMN_FOR_PROJECTION ||
		result == NULL || result->struct_size < TESS_DATUM_COLUMN_MIN_SIZE)
		elog(ERROR, "invalid Tessera test column request");

	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		if (!tess_row_mask_contains(&batch->rows, row))
			elog(ERROR, "Tessera test requested an inactive row");
		data->materialized[column][row / 64] |=
			UINT64CONST(1) << (row % 64);
	}
	data->purpose_calls[purpose]++;
	result->values = data->values[column];
	result->isnull = data->isnull[column];
	result->nrows = TEST_NROWS;
}

static void
test_release(TessBatch *batch)
{
	TestBatchData *data = batch->private_data;

	data->release_calls++;
}

static const TessBatchOps test_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = test_get_datum_column,
	.release = test_release,
};

Datum
tessera_test_batch(PG_FUNCTION_ARGS)
{
	TestBatchData data = {0};
	uint64		active_bits[2] = {UINT64_MAX, UINT64CONST(0x3f)};
	TessBatch batch = {
		TESS_ABI_INITIALIZER(TESS_BATCH_ABI_VERSION, TessBatch),
		.rows = {TEST_NROWS, active_bits},
		.table_oid = 42,
		.ops = &test_ops,
		.private_data = &data,
	};
	uint64		filter_bits[2] = {
		UINT64CONST(1) << 1,
		UINT64CONST(1) << 1,
	};
	const TessRowMask filter_rows = {TEST_NROWS, filter_bits};
	uint64		project_bits[2] = {UINT64CONST(1) << 1, 0};
	const TessRowMask project_rows = {TEST_NROWS, project_bits};
	TessDatumColumn filter_column =
		TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessDatumColumn project_column =
		TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessBatchOps short_ops = test_ops;
	int			column;
	int			row;

	for (column = 0; column < TEST_NCOLUMNS; column++)
	{
		for (row = 0; row < TEST_NROWS; row++)
			data.values[column][row] = Int32GetDatum(column * 1000 + row);
	}
	data.isnull[0][65] = true;

	short_ops.struct_size = TESS_BATCH_OPS_MIN_SIZE;
	if (batch.abi_version != TESS_BATCH_ABI_VERSION ||
		batch.struct_size < TESS_BATCH_MIN_SIZE ||
		test_ops.abi_version != TESS_BATCH_OPS_ABI_VERSION ||
		test_ops.struct_size < TESS_BATCH_OPS_MIN_SIZE ||
		!TESS_ABI_HAS_FIELD(&test_ops, TessBatchOps, release) ||
		TESS_ABI_HAS_FIELD(&short_ops, TessBatchOps, release))
		PG_RETURN_BOOL(false);

	/* values_stored: false unless set, and false in a batch of an older size. */
	if (tess_batch_values_stored(&batch))
		PG_RETURN_BOOL(false);
	batch.values_stored = true;
	if (!tess_batch_values_stored(&batch))
		PG_RETURN_BOOL(false);
	batch.struct_size = TESS_BATCH_MIN_SIZE;
	if (tess_batch_values_stored(&batch))
		PG_RETURN_BOOL(false);
	batch.struct_size = sizeof(TessBatch);

	batch.ops->get_datum_column(&batch, 0, &filter_rows,
		TESS_COLUMN_FOR_FILTER, &filter_column);
	if (filter_column.struct_size != sizeof(filter_column) ||
		filter_column.nrows != TEST_NROWS ||
		DatumGetInt32(filter_column.values[1]) != 1 ||
		filter_column.isnull[1] || !filter_column.isnull[65] ||
		data.materialized[0][0] != (UINT64CONST(1) << 1) ||
		data.materialized[0][1] != (UINT64CONST(1) << 1) ||
		data.purpose_calls[TESS_COLUMN_FOR_FILTER] != 1)
		PG_RETURN_BOOL(false);

	batch.ops->get_datum_column(&batch, 1, &project_rows,
		TESS_COLUMN_FOR_PROJECTION, &project_column);
	if (DatumGetInt32(project_column.values[1]) != 1001 ||
		DatumGetInt32(filter_column.values[1]) != 1 ||
		data.materialized[1][0] != (UINT64CONST(1) << 1) ||
		data.materialized[1][1] != 0 ||
		data.purpose_calls[TESS_COLUMN_FOR_PROJECTION] != 1)
		PG_RETURN_BOOL(false);

	batch.ops->release(&batch);
	PG_RETURN_BOOL(data.release_calls == 1 && batch.table_oid == 42 &&
		batch.private_data == &data);
}

/* The bytes form of the test's string column, two rows "ab" and "". */
static const char *const test_bytes_data[TEST_NROWS] = {"ab", ""};
static const uint32 test_bytes_lengths[TEST_NROWS] = {2, 0};
static const bool test_bytes_isnull[TEST_NROWS] = {false, false, true};

static bool
test_get_bytes_column(TessBatch *batch, int column, const TessRowMask *rows,
					  TessBytesColumn *result)
{
	if (rows == NULL || rows->nrows != batch->rows.nrows || result == NULL)
		elog(ERROR, "invalid Tessera test bytes request");
	if (column != 1 || result->struct_size < TESS_BYTES_COLUMN_MIN_SIZE)
		return false;
	result->nrows = TEST_NROWS;
	result->isnull = test_bytes_isnull;
	result->data = test_bytes_data;
	result->lengths = test_bytes_lengths;
	return true;
}

static const TessBatchOps test_bytes_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = test_get_datum_column,
	.release = test_release,
	.get_bytes_column = test_get_bytes_column,
};

/*
 * tess_batch_bytes_column: the operation's answer where the operations
 * have it, false for operations of a size without it and for one left
 * NULL, the result initialized with its size in every case.
 */
Datum
tessera_test_batch_bytes(PG_FUNCTION_ARGS)
{
	TestBatchData data = {0};
	uint64		active_bits[2] = {UINT64CONST(0x7), 0};
	TessBatch	batch = {
		TESS_ABI_INITIALIZER(TESS_BATCH_ABI_VERSION, TessBatch),
		.rows = {TEST_NROWS, active_bits},
		.ops = &test_bytes_ops,
		.private_data = &data,
	};
	TessBytesColumn bytes;
	/* Operations of the size before the field, allocated at that size. */
	Size		short_size = TESS_ABI_SIZE_INCLUDING_FIELD(TessBatchOps, release);
	TessBatchOps *short_ops = palloc(short_size);
	TessBatchOps null_ops = test_bytes_ops;
	const TessBytesColumn zero = TESS_STRUCT_INITIALIZER(TessBytesColumn);

	if (!tess_batch_bytes_column(&batch, 1, &batch.rows, &bytes) ||
		bytes.struct_size != sizeof(TessBytesColumn) ||
		bytes.nrows != TEST_NROWS || bytes.lengths[0] != 2 ||
		memcmp(bytes.data[0], "ab", 2) != 0 ||
		bytes.lengths[1] != 0 || bytes.data[1] == NULL ||
		!bytes.isnull[2] || bytes.indexes != NULL ||
		bytes.nvalues != 0 || bytes.dictionary != 0)
		PG_RETURN_BOOL(false);
	/*
	 * A column the provider does not offer, operations of the size before
	 * the field, and the field left NULL: false, each over a result filled
	 * with bytes of no meaning, which the helper initializes.
	 */
	memset(&bytes, 0x5a, sizeof(bytes));
	if (tess_batch_bytes_column(&batch, 0, &batch.rows, &bytes) ||
		bytes.struct_size != sizeof(TessBytesColumn))
		PG_RETURN_BOOL(false);
	memcpy(short_ops, &test_bytes_ops, short_size);
	short_ops->struct_size = short_size;
	batch.ops = short_ops;
	memset(&bytes, 0x5a, sizeof(bytes));
	if (tess_batch_bytes_column(&batch, 1, &batch.rows, &bytes) ||
		memcmp(&bytes, &zero, sizeof(bytes)) != 0)
		PG_RETURN_BOOL(false);
	null_ops.get_bytes_column = NULL;
	batch.ops = &null_ops;
	memset(&bytes, 0x5a, sizeof(bytes));
	PG_RETURN_BOOL(!tess_batch_bytes_column(&batch, 1, &batch.rows, &bytes) &&
				   memcmp(&bytes, &zero, sizeof(bytes)) == 0);
}

/* A dictionary identity taken through the API by a module of its own. */
Datum
tessera_test_batch_dictionary_identity(PG_FUNCTION_ARGS)
{
	const TessApi *api = *find_rendezvous_variable(TESS_API_RENDEZVOUS);

	PG_RETURN_INT64((int64) tess_dictionary_identity(api));
}

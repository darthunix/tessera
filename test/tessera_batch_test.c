#include "postgres.h"

#include "fmgr.h"

#include "tessera/batch.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_batch);

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

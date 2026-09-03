#include "postgres.h"

#include "fmgr.h"
#include "nodes/bitmapset.h"

#include "tessera/layout.h"
#include "tessera/request.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_request);

Datum
tessera_test_request(PG_FUNCTION_ARGS)
{
	const int	target_columns[] = {0, -1, 1};
	TessLayout identity = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 70,
		.ntargets = 3,
	};
	TessLayout mapped = {
		.struct_size = sizeof(TessLayout),
		.ncolumns = 2,
		.ntargets = 3,
		.target_columns = target_columns,
	};
	Bitmapset  *filters = NULL;
	Bitmapset  *projections = NULL;
	TessRequest request;
	bool		result;

	filters = bms_add_member(filters, 2);
	filters = bms_add_member(filters, 65);
	projections = bms_add_member(projections, 0);
	projections = bms_add_member(projections, 65);
	request = (TessRequest) {
		.struct_size = sizeof(TessRequest),
		.filter_columns = filters,
		.projection_columns = projections,
		.output_mode = TESS_OUTPUT_BATCH,
		.max_batch_rows = 64,
	};

	result = identity.struct_size >= TESS_LAYOUT_MIN_SIZE &&
		mapped.struct_size >= TESS_LAYOUT_MIN_SIZE &&
		request.struct_size >= TESS_REQUEST_MIN_SIZE &&
		tess_layout_column(&identity, 2) == 2 &&
		tess_layout_column(&mapped, 0) == 0 &&
		tess_layout_column(&mapped, 1) == -1 &&
		tess_layout_column(&mapped, 2) == 1 &&
		bms_is_member(2, request.filter_columns) &&
		bms_is_member(65, request.filter_columns) &&
		!bms_is_member(0, request.filter_columns) &&
		bms_is_member(0, request.projection_columns) &&
		bms_is_member(65, request.projection_columns) &&
		request.output_mode == TESS_OUTPUT_BATCH &&
		request.max_batch_rows == 64;

	request.output_mode = TESS_OUTPUT_ROWS;
	request.max_batch_rows = 0;
	result = result && request.output_mode == TESS_OUTPUT_ROWS &&
		request.max_batch_rows == 0;
	bms_free(filters);
	bms_free(projections);

	PG_RETURN_BOOL(result);
}

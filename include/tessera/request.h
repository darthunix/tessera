/* Column and output requirements sent from a consumer to its producer. */
#ifndef TESSERA_REQUEST_H
#define TESSERA_REQUEST_H

#include "postgres.h"

#include "nodes/bitmapset.h"

#include "tessera/abi.h"

/* How the consumer wants the producer to expose its results. */
typedef enum TessOutputMode
{
	/* Expose one ordinary PostgreSQL row at a time. */
	TESS_OUTPUT_ROWS,
	/* Expose a batch through the returned TupleTableSlot. */
	TESS_OUTPUT_BATCH
} TessOutputMode;

/*
 * Requirements for one producer-to-consumer connection.
 *
 * Column masks use zero-based TessLayout column numbers. NULL is an empty
 * mask. Neither mask is owned by this structure.
 */
typedef struct TessRequest
{
	Size		struct_size;
	/* Columns needed before the active row set is final. */
	const Bitmapset *filter_columns;
	/* Columns needed only for rows that survive filtering. */
	const Bitmapset *projection_columns;
	TessOutputMode output_mode;
	/* Maximum physical rows in one batch, or zero for no explicit limit. */
	int			max_batch_rows;
} TessRequest;

#define TESS_REQUEST_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessRequest, max_batch_rows)

#endif /* TESSERA_REQUEST_H */

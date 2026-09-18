/* C entry points of the Rust kernels over Datum columns and row masks. */
#ifndef TESSERA_KERNELS_H
#define TESSERA_KERNELS_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/row_mask.h"

/*
 * The kernels are implemented in Rust (crates/tessera-capi) and linked as a
 * static library. Every entry point reads a column as PostgreSQL Datum
 * values with NULL flags (TessDatumColumn), reads or narrows row masks, and
 * returns a status: it never raises ERROR, so the caller reports the status
 * with ereport after the call returns. See docs/kernels.md.
 *
 * prepared names the mask the column was obtained with (every row in it is
 * initialized, a NULL row by a placeholder); NULL means the whole column is
 * initialized. Kernels may read every row of a 64-row word, so the mask
 * matters when unrequested rows are uninitialized. The rows a kernel
 * selects must lie within prepared, or the call fails.
 *
 * Buffers must not alias: a mutable mask or array passed to a call must not
 * overlap prepared, the column's arrays, or another mutable argument, and
 * the column must not change during the call. Every mask has the column's
 * row count and no bits set beyond it. After a failure the mutable outputs
 * of the call hold unspecified values and must not be used; results
 * returned through plain pointers are written only on success.
 */

#define TESS_KERNELS_ABI_VERSION 0

/* The outcome of an entry point; the same value is stored in TessStatus. */
typedef enum TessStatusCode
{
	TESS_OK = 0,
	/* A dimension, pointer, mask or operation argument is invalid. */
	TESS_ERROR_INVALID_ARGUMENT = 1,
	/* SQLSTATE 22003: an int4 result does not fit. */
	TESS_ERROR_INTEGER_OUT_OF_RANGE = 2,
	/* SQLSTATE 22012: a zero divisor. */
	TESS_ERROR_DIVISION_BY_ZERO = 3,
	/* A Rust panic was caught; the library remains usable. */
	TESS_ERROR_PANIC = 4
} TessStatusCode;

#define TESS_STATUS_MESSAGE_SIZE 120

/*
 * Details of an outcome. The caller initializes struct_size with
 * TESS_STRUCT_INITIALIZER; an entry point fills the other fields, leaving a
 * NULL or undersized status alone. sqlstate is a five-character code with a
 * terminator ("XX000" for invalid arguments and panics), message a
 * NUL-terminated text; both are empty on success.
 */
typedef struct TessStatus
{
	Size		struct_size;
	TessStatusCode code;
	char		sqlstate[6];
	char		message[TESS_STATUS_MESSAGE_SIZE];
} TessStatus;

#define TESS_STATUS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessStatus, message)

/* A comparison of column values with a scalar on the right. */
typedef enum TessCompareOp
{
	TESS_CMP_EQ = 0,
	TESS_CMP_NE = 1,
	TESS_CMP_LT = 2,
	TESS_CMP_LE = 3,
	TESS_CMP_GT = 4,
	TESS_CMP_GE = 5
} TessCompareOp;

/* Sizes and offsets the Rust side was built with, for layout checks. */
typedef enum TessLayoutKind
{
	TESS_LAYOUT_ROW_MASK_SIZE = 0,
	TESS_LAYOUT_DATUM_COLUMN_SIZE = 1,
	TESS_LAYOUT_DATUM_COLUMN_NROWS_OFFSET = 2,
	TESS_LAYOUT_STATUS_SIZE = 3,
	TESS_LAYOUT_STATUS_MESSAGE_OFFSET = 4
} TessLayoutKind;

/* The ABI version the library implements; must equal the header's. */
extern uint32 tess_kernels_abi_version(void);

/* The size or offset for kind, or 0 for an unknown kind. */
extern Size tess_kernels_layout(TessLayoutKind kind);

/*
 * Raise and catch a panic, returning TESS_ERROR_PANIC: verifies that the
 * linked library unwinds panics into statuses instead of aborting.
 */
extern TessStatusCode tess_kernels_test_panic(TessStatus *status);

/*
 * Keep in rows only the selected rows whose non-NULL int4 value satisfies
 * `value op scalar`. NULL values never satisfy a comparison.
 */
extern TessStatusCode tess_int4_filter(const TessDatumColumn *column,
									   const TessRowMask *prepared,
									   TessRowMask *rows,
									   TessCompareOp op,
									   int32 scalar,
									   TessStatus *status);

#endif							/* TESSERA_KERNELS_H */

/* C entry points of the Rust kernels over Datum columns and row masks. */
#ifndef TESSERA_KERNELS_H
#define TESSERA_KERNELS_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/row_mask.h"
#include "tessera/status.h"

/*
 * The kernels are implemented in Rust (crates/tessera-capi) and linked as a
 * static library. Every entry point reads a column as PostgreSQL Datum
 * values with NULL flags (TessDatumColumn), reads or narrows row masks, and
 * returns a status (tessera/status.h): it never raises ERROR, so the caller
 * reports the status with ereport after the call returns. See docs/kernels.md.
 *
 * prepared names the mask the column was obtained with (every row in it is
 * initialized, a NULL row by a placeholder); NULL means the whole column is
 * initialized. Kernels may read every row of a 64-row word, so the mask
 * matters when unrequested rows are uninitialized. The rows a kernel
 * selects must lie within prepared, or the call fails.
 *
 * Buffers must not alias: a mutable mask or array passed to a call must not
 * overlap prepared, the column's arrays, or another mutable argument, and
 * the column must not change during the call. Every mask, result masks
 * included, has the column's row count and no bits set beyond it on entry;
 * the row bits of a result mask may hold anything. After a failure the mutable outputs
 * of the call hold unspecified values and must not be used; results
 * returned through plain pointers are written only on success.
 */

#define TESS_KERNELS_ABI_VERSION 0

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

/* A binary int4 operation with PostgreSQL's semantics and error codes. */
typedef enum TessArithOp
{
	TESS_ARITH_ADD = 0,
	TESS_ARITH_SUB = 1,
	TESS_ARITH_MUL = 2,
	/* Truncating toward zero; MIN / -1 is out of range. */
	TESS_ARITH_DIV = 3,
	/* With the dividend's sign; x % -1 is 0. */
	TESS_ARITH_MOD = 4
} TessArithOp;

/* What a NULL key does to its row when hashing. */
typedef enum TessNullKeys
{
	/* The row leaves the valid mask: a NULL key matches nothing in a join. */
	TESS_NULL_KEYS_REJECT = 0,
	/* The row stays and NULL hashes as a fixed key: NULLs form one group. */
	TESS_NULL_KEYS_GROUP = 1
} TessNullKeys;

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

/*
 * Keep in rows only the selected rows where both int4 columns are non-NULL
 * and `left op right`, each column read with its own readiness mask.
 */
extern TessStatusCode tess_int4_compare_columns(const TessDatumColumn *left,
												const TessRowMask *left_prepared,
												const TessDatumColumn *right,
												const TessRowMask *right_prepared,
												TessRowMask *rows,
												TessCompareOp op,
												TessStatus *status);

/*
 * The int8 sum of the selected non-NULL values; without any, isnull is set
 * and sum is 0. The sum of one batch cannot overflow; adding batches may,
 * which the caller checks.
 */
extern TessStatusCode tess_int4_sum(const TessDatumColumn *column,
									const TessRowMask *prepared,
									const TessRowMask *rows,
									bool *isnull,
									int64 *sum,
									TessStatus *status);

/* The least selected non-NULL value; without any, isnull is set. */
extern TessStatusCode tess_int4_min(const TessDatumColumn *column,
									const TessRowMask *prepared,
									const TessRowMask *rows,
									bool *isnull,
									int32 *value,
									TessStatus *status);

/* The greatest selected non-NULL value; without any, isnull is set. */
extern TessStatusCode tess_int4_max(const TessDatumColumn *column,
									const TessRowMask *prepared,
									const TessRowMask *rows,
									bool *isnull,
									int32 *value,
									TessStatus *status);

/*
 * Arithmetic into a dense int4 result: for every word with selected rows,
 * non_nulls gets the selected rows whose operands are non-NULL and values
 * gets their results (a NULL row an initialized placeholder); words without
 * selected rows get a cleared non_nulls word; rows outside the selection
 * are unspecified and may stay uninitialized, so values needs no
 * initialization. Overflow fails with TESS_ERROR_INTEGER_OUT_OF_RANGE and a
 * zero divisor with TESS_ERROR_DIVISION_BY_ZERO; a NULL operand never fails.
 * After a failure values and non_nulls are unspecified.
 */
extern TessStatusCode tess_int4_arith_scalar(TessArithOp op,
											 const TessDatumColumn *column,
											 int32 scalar,
											 const TessRowMask *prepared,
											 const TessRowMask *rows,
											 int32 *values,
											 TessRowMask *non_nulls,
											 TessStatus *status);

/* scalar op column, for the operations where the order matters. */
extern TessStatusCode tess_int4_arith_scalar_left(TessArithOp op,
												  int32 scalar,
												  const TessDatumColumn *column,
												  const TessRowMask *prepared,
												  const TessRowMask *rows,
												  int32 *values,
												  TessRowMask *non_nulls,
												  TessStatus *status);

/* left op right row by row; a NULL on either side makes a NULL. */
extern TessStatusCode tess_int4_arith_columns(TessArithOp op,
											  const TessDatumColumn *left,
											  const TessRowMask *left_prepared,
											  const TessDatumColumn *right,
											  const TessRowMask *right_prepared,
											  const TessRowMask *rows,
											  int32 *values,
											  TessRowMask *non_nulls,
											  TessStatus *status);

/*
 * Hash the first key of the selected rows: hashes[row] receives
 * murmurhash32 of the row's int4 value (32 bits, as pg_batch) and valid
 * receives the selection narrowed by the NULL policy. hashes has the
 * batch's row count and any contents; rows outside valid are unspecified.
 */
extern TessStatusCode tess_int4_hash(const TessDatumColumn *column,
									 const TessRowMask *prepared,
									 const TessRowMask *rows,
									 TessNullKeys nulls,
									 uint32 *hashes,
									 TessRowMask *valid,
									 TessStatus *status);

/*
 * Fold the next key into the hashes of the rows in valid with
 * hash_combine, narrowing valid by the NULL policy; a row rejected by an
 * earlier key is not read. Keys fold in call order.
 */
extern TessStatusCode tess_int4_hash_next(const TessDatumColumn *column,
										  const TessRowMask *prepared,
										  TessNullKeys nulls,
										  uint32 *hashes,
										  TessRowMask *valid,
										  TessStatus *status);

/*
 * The int8 family: the same kernels over columns whose Datum holds an
 * int8 (the whole word, as DatumGetInt64 reads it), with int64 scalars,
 * values and results.
 */

/* Keep in rows only the selected rows whose non-NULL int8 value satisfies `value op scalar`. */
extern TessStatusCode tess_int8_filter(const TessDatumColumn *column,
									   const TessRowMask *prepared,
									   TessRowMask *rows,
									   TessCompareOp op,
									   int64 scalar,
									   TessStatus *status);

/* As tess_int4_compare_columns, over two int8 columns. */
extern TessStatusCode tess_int8_compare_columns(const TessDatumColumn *left,
												const TessRowMask *left_prepared,
												const TessDatumColumn *right,
												const TessRowMask *right_prepared,
												TessRowMask *rows,
												TessCompareOp op,
												TessStatus *status);

/*
 * Arithmetic into a dense int8 result, as tess_int4_arith_scalar and its
 * siblings: overflow fails with TESS_ERROR_INTEGER_OUT_OF_RANGE (SQLSTATE
 * 22003, "bigint out of range"), a zero divisor with
 * TESS_ERROR_DIVISION_BY_ZERO, MIN / -1 is out of range and x % -1 is 0.
 */
extern TessStatusCode tess_int8_arith_scalar(TessArithOp op,
											 const TessDatumColumn *column,
											 int64 scalar,
											 const TessRowMask *prepared,
											 const TessRowMask *rows,
											 int64 *values,
											 TessRowMask *non_nulls,
											 TessStatus *status);

extern TessStatusCode tess_int8_arith_scalar_left(TessArithOp op,
												  int64 scalar,
												  const TessDatumColumn *column,
												  const TessRowMask *prepared,
												  const TessRowMask *rows,
												  int64 *values,
												  TessRowMask *non_nulls,
												  TessStatus *status);

extern TessStatusCode tess_int8_arith_columns(TessArithOp op,
											  const TessDatumColumn *left,
											  const TessRowMask *left_prepared,
											  const TessDatumColumn *right,
											  const TessRowMask *right_prepared,
											  const TessRowMask *rows,
											  int64 *values,
											  TessRowMask *non_nulls,
											  TessStatus *status);

/*
 * The number of selected non-NULL rows of a column of any type: only the
 * NULL flags are read, so a count needs no width.
 */
extern TessStatusCode tess_count(const TessDatumColumn *column,
								 const TessRowMask *prepared,
								 const TessRowMask *rows,
								 int64 *count,
								 TessStatus *status);

/*
 * Key hashes over int8 columns, as tess_int4_hash and tess_int4_hash_next:
 * the value is folded to 32 bits as PostgreSQL's hashint8 folds it (low
 * half xor high half, the high half inverted for negative values) before
 * murmurhash32. An int8 inside the int4 range hashes exactly like the int4
 * of the same value, so int4 and int8 keys of one join or grouping share a
 * table, and the calls of both families chain in any order.
 */
extern TessStatusCode tess_int8_hash(const TessDatumColumn *column,
									 const TessRowMask *prepared,
									 const TessRowMask *rows,
									 TessNullKeys nulls,
									 uint32 *hashes,
									 TessRowMask *valid,
									 TessStatus *status);

extern TessStatusCode tess_int8_hash_next(const TessDatumColumn *column,
										  const TessRowMask *prepared,
										  TessNullKeys nulls,
										  uint32 *hashes,
										  TessRowMask *valid,
										  TessStatus *status);

/* The least selected non-NULL int8 value; without any, isnull is set. */
extern TessStatusCode tess_int8_min(const TessDatumColumn *column,
									const TessRowMask *prepared,
									const TessRowMask *rows,
									bool *isnull,
									int64 *value,
									TessStatus *status);

/* The greatest selected non-NULL int8 value; without any, isnull is set. */
extern TessStatusCode tess_int8_max(const TessDatumColumn *column,
									const TessRowMask *prepared,
									const TessRowMask *rows,
									bool *isnull,
									int64 *value,
									TessStatus *status);

/*
 * The selected int4 values widened into int8 Datums, with the output
 * contract of the arithmetic: non_nulls gets the selected rows whose value
 * is non-NULL and values gets their int8 Datums; a NULL row an initialized
 * placeholder, rows outside the selection unspecified.
 */
extern TessStatusCode tess_int4_to_int8(const TessDatumColumn *column,
										const TessRowMask *prepared,
										const TessRowMask *rows,
										Datum *values,
										TessRowMask *non_nulls,
										TessStatus *status);

/*
 * The selected int8 values narrowed into int32 values, with the same
 * output contract; a selected non-NULL value outside the int4 range fails
 * with TESS_ERROR_INTEGER_OUT_OF_RANGE, 22003 "integer out of range", as
 * the cast int4(bigint).
 */
extern TessStatusCode tess_int8_to_int4(const TessDatumColumn *column,
										const TessRowMask *prepared,
										const TessRowMask *rows,
										int32 *values,
										TessRowMask *non_nulls,
										TessStatus *status);

#endif							/* TESSERA_KERNELS_H */

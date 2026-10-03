/*
 * Decimals: numeric values of at most 18 digits as an int64 at their
 * display scale, read straight from the stored form of numeric, computed
 * exactly and written back as the core's make_result writes them. The C
 * entry points of the Rust kernels (tessera_kernels::decimal), for the
 * batch functions of numeric, a heap batch's decimals and the aggregates
 * that fold numeric values themselves; modules that do not link the
 * kernels reach the ones they need through TessKernelOps.
 *
 * A numeric is read in place from a varlena of either header: after it, a
 * short header word (0x8000 set: 0x2000 the sign, 0x1F80 the display
 * scale, 0x0040 and 0x003F the weight) or a long one (the sign in 0xC000,
 * the display scale in 0x3FFF, then an int16 weight), then the digits of
 * base 10000 from the highest. A compressed or external value, NaN, an
 * infinity, a display scale past 18 and a value of more than 18 digits are
 * not decimals: the calls leave such a row to the caller, which takes it
 * by the core's function. A result keeps the core's scale: + and - the
 * larger, * the sum (up to 36), a cast to an integer rounds half away from
 * zero.
 *
 * The calls follow tessera/kernels.h: they never raise ERROR nor call
 * PostgreSQL, and return a status. They read the selected rows alone: the
 * NULL flag of each must be initialized, and a non-NULL value must be a
 * numeric Datum pointing to a whole varlena or, in the column's decimal
 * side (TessDatumColumn.decimal_rows), the decimal itself. Every mask has
 * the row count of the selection, and so has every array.
 */
#ifndef TESSERA_DECIMAL_H
#define TESSERA_DECIMAL_H

#include "postgres.h"

#include "tessera/kernels.h"
#include "tessera/table.h"

/* The most digits of a decimal. */
#define TESS_DECIMAL_DIGITS 18

/* The most bytes of a numeric the calls write, varlena header included. */
#define TESS_DECIMAL_NUMERIC_MAX 32

/* An argument: a column, or a scalar numeric (never NULL) without one. */
typedef struct TessDecimalArg
{
	const TessDatumColumn *column;
	Datum		scalar;
} TessDecimalArg;

/* An operation of tess_decimal_compute. */
typedef enum TessDecimalOp
{
	TESS_DECIMAL_ADD = 0,
	TESS_DECIMAL_SUB = 1,
	TESS_DECIMAL_MUL = 2,
	TESS_DECIMAL_NEGATE = 3,
	TESS_DECIMAL_ABS = 4
} TessDecimalOp;

/*
 * Narrow rows to the rows where the comparison of two decimals holds, in
 * numeric_cmp's exact order. A row with a NULL argument leaves; a row whose
 * arguments are not both decimals leaves too and is set in rest, whose
 * other bits are cleared, for the caller to compare by the core.
 */
extern TessStatusCode tess_decimal_filter(TessCompareOp op,
										  const TessDecimalArg *left,
										  const TessDecimalArg *right,
										  TessRowMask *rows,
										  TessRowMask *rest,
										  TessStatus *status);

/*
 * An operation over the selected rows. non_nulls gets the rows without a
 * NULL argument, rest the ones of them left to the caller (an argument
 * that is not a decimal, a result past 18 digits), and every other row of
 * non_nulls its result: the value in values, the scale in scales, and,
 * when the scale is scale (not negative), its bit in decimals, whose other
 * bits are cleared. right is ignored by NEGATE and ABS. Values and scales
 * of other rows are unspecified.
 */
extern TessStatusCode tess_decimal_compute(TessDecimalOp op,
										   const TessDecimalArg *left,
										   const TessDecimalArg *right,
										   const TessRowMask *rows,
										   int scale,
										   int64 *values,
										   uint8 *scales,
										   TessRowMask *non_nulls,
										   TessRowMask *decimals,
										   TessRowMask *rest,
										   TessStatus *status);

/*
 * int4(numeric) over the selected rows, rounded half away from zero, with
 * the masks of tess_decimal_compute: a row that is not a decimal, or whose
 * integer passes the int4 range, is left in rest, where the core's
 * function raises its error.
 */
extern TessStatusCode tess_decimal_to_int4(const TessDecimalArg *arg,
										   const TessRowMask *rows,
										   int32 *values,
										   TessRowMask *non_nulls,
										   TessRowMask *rest,
										   TessStatus *status);

/* int8(numeric), as tess_decimal_to_int4; every decimal's integer fits. */
extern TessStatusCode tess_decimal_to_int8(const TessDecimalArg *arg,
										   const TessRowMask *rows,
										   int64 *values,
										   TessRowMask *non_nulls,
										   TessRowMask *rest,
										   TessStatus *status);

/*
 * Read the selected rows of a numeric column as decimals: a row taken gets
 * the Datum of its int64 in values and its bit set in decimals; every
 * other selected row (NULL, not a decimal, of another scale) gets its
 * Datum copied into values and its bit cleared; the bits of rows outside
 * the selection stay. With scales, every decimal is taken and its scale
 * written there; without, the decimals of *scale, the first decimal's
 * when it is -1, which is left there. The column's decimal side is read as
 * decimals.
 */
extern TessStatusCode tess_decimal_read(const TessDatumColumn *column,
										const TessRowMask *rows,
										int *scale,
										Datum *values,
										uint8 *scales,
										TessRowMask *decimals,
										TessStatus *status);

/*
 * A running sum of decimals as the core's numeric sum and avg keep it: the
 * int128 value (high * 2^64 + low) at the largest display scale met,
 * below 10^36 in magnitude, and the decimals added.
 */
typedef struct TessDecimalSum
{
	uint64		low;
	int64		high;
	int			scale;
	int64		count;
} TessDecimalSum;

/*
 * Add the selected rows' values to *sum as decimals, NULL rows skipped: a
 * column of kind TESS_TABLE_SUM_OF_NUMERIC, _INT4 (int2 too) or _INT8,
 * an integer a decimal at scale 0. A row that is not a decimal, or whose
 * addition (or the sum's rescaling to its scale) would reach 10^36, is set
 * in rest, whose other bits are cleared, for the caller to add by the
 * core's means. *sum is written on success only; a sum past its bound or
 * of a scale past 18, or another kind, is an invalid argument.
 */
extern TessStatusCode tess_decimal_sum(TessTableSumInput kind,
									   const TessDatumColumn *column,
									   const TessRowMask *rows,
									   TessDecimalSum *sum,
									   TessRowMask *rest,
									   TessStatus *status);

/*
 * Replace each selected row's decimal in values (an int64 at its scale in
 * scales, or at scale for every row when scales is NULL) by the pointer to
 * its numeric, written into space one after another at MAXALIGN'd
 * offsets. space must be MAXALIGN'd; TESS_DECIMAL_NUMERIC_MAX bytes a row
 * always suffice, and *used gets the bytes taken.
 */
extern TessStatusCode tess_decimal_write(Datum *values,
										 const uint8 *scales,
										 int scale,
										 const TessRowMask *rows,
										 void *space,
										 Size len,
										 Size *used,
										 TessStatus *status);

/* A numeric Datum as a decimal; *found is false when it is none. */
extern TessStatusCode tess_decimal_read_datum(Datum datum,
											  int64 *value,
											  int *scale,
											  bool *found,
											  TessStatus *status);

/*
 * The numeric of value / 10^scale with that display scale into out, len
 * bytes of at least TESS_DECIMAL_NUMERIC_MAX aligned for an int32; *size
 * gets its size. A value of more than 18 digits or a scale outside 0..36
 * is an invalid argument.
 */
extern TessStatusCode tess_decimal_write_datum(int64 value,
											   int scale,
											   void *out,
											   Size len,
											   Size *size,
											   TessStatus *status);

#endif							/* TESSERA_DECIMAL_H */

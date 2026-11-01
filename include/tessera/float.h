/*
 * The float functions: the C entry points of the Rust kernels
 * (tessera_kernels::float) for the comparisons, arithmetic and casts of
 * float4 and float8 of kernels/float.c. A float8 is its Datum's 64 bits, a
 * float4 the low 32 bits of its Datum (a float4 result is written as
 * Float4GetDatum writes it, those bits as an int32 sign-extended), an int4
 * (an int2 too, its value sign-extended) the low 32 bits, and an int8 the
 * whole Datum.
 *
 * The rules are the core's of utils/float.h: NaN is equal to itself and
 * greater than every number, -0 equals 0; an infinite result of finite
 * operands overflows, a zero product of non-zero operands or a zero
 * quotient of a non-zero dividend by a finite divisor underflows, a zero
 * divisor of a dividend that is not NaN divides by zero; a float becomes
 * an integer rounded half to even and checked in its own width, a float8
 * becomes a float4 checked as dtof checks it. A float4 operand of a float8
 * operation is widened. Each call fails where the core's function raises,
 * with TESS_ERROR_DATA_EXCEPTION and the core's message and SQLSTATE
 * (22003, or 22012 for a division by zero) in the status, at the first row
 * the core would.
 *
 * The calls follow tessera/kernels.h: they never raise ERROR nor call
 * PostgreSQL, and return a status. They read the selected rows alone,
 * whose NULL flags and non-NULL values must be initialized; every mask
 * has the selection's row count, and so has every array. A result mask
 * gets every word written: the selected rows without NULL.
 */
#ifndef TESSERA_FLOAT_H
#define TESSERA_FLOAT_H

#include "postgres.h"

#include "tessera/kernels.h"

/* An argument: a column, or a scalar Datum without one. */
typedef struct TessFloatArg
{
	const TessDatumColumn *column;
	Datum		scalar;
} TessFloatArg;

/*
 * How a Datum holds a number. An int2 argument is read as
 * TESS_FLOAT_TYPE_INT4; TESS_FLOAT_TYPE_INT2 names a cast's result.
 */
typedef enum TessFloatType
{
	TESS_FLOAT_TYPE_FLOAT8 = 0,
	TESS_FLOAT_TYPE_FLOAT4 = 1,
	TESS_FLOAT_TYPE_INT4 = 2,
	TESS_FLOAT_TYPE_INT2 = 3,
	TESS_FLOAT_TYPE_INT8 = 4
} TessFloatType;

/* An operation of tess_float_arith; the last two take one argument. */
typedef enum TessFloatOp
{
	TESS_FLOAT_ADD = 0,
	TESS_FLOAT_SUB = 1,
	TESS_FLOAT_MUL = 2,
	TESS_FLOAT_DIV = 3,
	TESS_FLOAT_NEG = 4,
	TESS_FLOAT_ABS = 5
} TessFloatOp;

/*
 * left op right of two floats, float8 or float4 in any pair: rows keeps the
 * selected rows where both are not NULL and the comparison holds.
 */
extern TessStatusCode tess_float_compare(TessCompareOp op,
										 TessFloatType left_type,
										 const TessFloatArg *left,
										 TessFloatType right_type,
										 const TessFloatArg *right,
										 TessRowMask *rows,
										 TessStatus *status);

/*
 * left op right of two floats, or op left for TESS_FLOAT_NEG and
 * TESS_FLOAT_ABS (right and right_type unread, right may be NULL): a float4
 * result when both are float4, a float8 result otherwise, written as
 * Datums to the selected rows without NULL.
 */
extern TessStatusCode tess_float_arith(TessFloatOp op,
									   TessFloatType left_type,
									   const TessFloatArg *left,
									   TessFloatType right_type,
									   const TessFloatArg *right,
									   const TessRowMask *rows,
									   Datum *values,
									   TessRowMask *non_nulls,
									   TessStatus *status);

/*
 * The cast of arg, of type from, to type to, one of them a float: values
 * holds int32 for an int4 or int2 result and Datums otherwise.
 */
extern TessStatusCode tess_float_cast(TessFloatType from,
									  TessFloatType to,
									  const TessFloatArg *arg,
									  const TessRowMask *rows,
									  void *values,
									  TessRowMask *non_nulls,
									  TessStatus *status);

#endif							/* TESSERA_FLOAT_H */

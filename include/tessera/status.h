/* The outcome of a Tessera call that must not raise ERROR. */
#ifndef TESSERA_STATUS_H
#define TESSERA_STATUS_H

#include "postgres.h"

#include "tessera/abi.h"

/* The outcome of a call; the same value is stored in TessStatus. */
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
	TESS_ERROR_PANIC = 4,
	/* Another error of the function's own, its SQLSTATE and message in the status. */
	TESS_ERROR_DATA_EXCEPTION = 5
} TessStatusCode;

#define TESS_STATUS_MESSAGE_SIZE 120

/*
 * Details of an outcome. The caller initializes struct_size with
 * TESS_STRUCT_INITIALIZER; the callee fills the other fields, leaving a NULL
 * or undersized status alone. sqlstate is a five-character code with a
 * terminator ("XX000" for invalid arguments and panics), message a
 * NUL-terminated text; both are empty on success. A caller reports a
 * failure with ereport after the call has returned.
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

#endif							/* TESSERA_STATUS_H */

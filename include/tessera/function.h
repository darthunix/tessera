/* Batch implementations of PostgreSQL functions, found by function OID. */
#ifndef TESSERA_FUNCTION_H
#define TESSERA_FUNCTION_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/batch.h"
#include "tessera/row_mask.h"
#include "tessera/status.h"

#define TESS_FUNCTION_ABI_VERSION 0
#define TESS_FUNCTION_REGISTRY_OPS_ABI_VERSION 0

/*
 * A TessFunction describes a batch implementation of one PostgreSQL function
 * (pg_proc), identified by its OID: the implementation returns the same
 * values and raises the same errors as the function on every input, so a
 * consumer may evaluate a FuncExpr or an OpExpr's function with it over a
 * whole batch instead of calling the function per row. The argument and
 * result types are those of the function in the catalog; the description
 * does not repeat them. See docs/function.md.
 *
 * The description is independent of the argument types: int4 kernels are
 * registered by the module that links the Rust kernels, and implementations
 * for other types, or from other extensions, use the same structure.
 */

/* What an implementation produces. */
typedef enum TessFunctionKind
{
	/* A boolean: keeps in the call's rows the selected rows where it is true. */
	TESS_FUNCTION_PREDICATE = 0,
	/* A column of the function's result type with a non-NULL mask. */
	TESS_FUNCTION_VALUE = 1
} TessFunctionKind;

/* How a VALUE implementation stores its result column. */
typedef enum TessResultFormat
{
	/* One Datum per row; by-reference values are allocated in the call's context. */
	TESS_RESULT_DATUM = 0,
	/* One int32 per row, for int4-compatible result types. */
	TESS_RESULT_INT32 = 1
} TessResultFormat;

/* A NULL argument yields NULL (a false predicate); required in this version. */
#define TESS_FUNCTION_STRICT 0x1
/* The input collation does not affect the result. */
#define TESS_FUNCTION_COLLATION_INSENSITIVE 0x2
/*
 * Every combination of column and scalar arguments is accepted. Without it,
 * args[0] is the only column and the others are scalars; a consumer moves the
 * column first through the operator's commutator from the catalog.
 */
#define TESS_FUNCTION_ANY_SHAPE 0x4

/* One argument of a call: a column of the batch or a scalar. */
typedef struct TessFunctionArg
{
	Size		struct_size;
	/* The column, or NULL for a scalar. */
	const TessDatumColumn *column;
	/* The readiness of the column (see tessera/kernels.h); NULL when whole. */
	const TessRowMask *prepared;
	/* The scalar when column is NULL; never NULL: the consumer folds those. */
	Datum		scalar;
} TessFunctionArg;

#define TESS_FUNCTION_ARG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessFunctionArg, scalar)

typedef struct TessFunction TessFunction;

/*
 * One evaluation over a batch. The consumer fills every field it uses and
 * initializes struct_size; the implementation reads the arguments and
 * writes the outputs of its kind. After a failure the mutable outputs are
 * unspecified; results are complete only on TESS_OK.
 */
typedef struct TessFunctionCall
{
	Size		struct_size;
	const TessFunction *function;
	int			nargs;
	const TessFunctionArg *args;
	Oid			inputcollid;
	/* The selected rows; a PREDICATE narrows the mask in place. */
	TessRowMask *rows;
	/* VALUE: the result array in the function's format, one slot per batch row. */
	void	   *values;
	/* VALUE: the rows whose result was written and is non-NULL. */
	TessRowMask *non_nulls;
	/* VALUE with TESS_RESULT_DATUM: where by-reference results are allocated. */
	MemoryContext context;
	/* Filled on failure, as by the kernels. */
	TessStatus *status;
} TessFunctionCall;

#define TESS_FUNCTION_CALL_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessFunctionCall, status)

/*
 * The description of one implementation. The provider owns it and keeps it
 * valid and unchanged from registration until removal; the registry stores
 * the pointer without copying.
 */
struct TessFunction
{
	uint32		abi_version;
	Size		struct_size;
	/* The pg_proc function this implementation is equivalent to. */
	Oid			funcid;
	TessFunctionKind kind;
	TessResultFormat result_format;
	/* TESS_FUNCTION_* flags. */
	uint32		flags;
	/* Evaluate one call; returns the code also stored in the call's status. */
	TessStatusCode (*evaluate) (TessFunctionCall *call);
};

#define TESS_FUNCTION_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessFunction, evaluate)

/* The registry of batch implementations, keyed by function OID. */
typedef struct TessFunctionRegistryOps
{
	uint32		abi_version;
	Size		struct_size;
	/*
	 * Register without copying. Repeating the same registration is safe;
	 * another description for the same function is an error, as is an
	 * invalid description.
	 */
	void		(*add) (const TessFunction *function);
	/*
	 * Unregister this exact description without freeing it or waiting for
	 * users. NULL and repeated calls for an object that is still alive are
	 * safe.
	 */
	void		(*remove) (const TessFunction *function);
	/*
	 * The implementation of the function with this OID, or NULL. The
	 * borrowed pointer does not extend the description's lifetime and must
	 * not be used after remove. The consumer must not modify or free it.
	 */
	const TessFunction *(*find) (Oid funcid);
} TessFunctionRegistryOps;

#define TESS_FUNCTION_REGISTRY_OPS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessFunctionRegistryOps, find)

#endif							/* TESSERA_FUNCTION_H */

/*
 * The built-in int4 kernels as batch functions: this module links the Rust
 * kernels and registers them in the bridge's function registry when loaded.
 * Load the bridge first (CREATE EXTENSION tessera), then LOAD 'tessera_kernels'.
 */
#include "postgres.h"

#include "fmgr.h"
#include "utils/fmgroids.h"

#include "tessera/bridge.h"
#include "tessera/kernels.h"

PG_MODULE_MAGIC;

PGDLLEXPORT void _PG_init(void);

/* A description with the kernel operation it maps to. */
typedef struct Int4Function
{
	TessFunction function;
	uint32		op;
} Int4Function;

static TessStatusCode compare_evaluate(TessFunctionCall *call);
static TessStatusCode arith_evaluate(TessFunctionCall *call);
static TessStatusCode negate_evaluate(TessFunctionCall *call);

#define INT4_COMPARE(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_PREDICATE, \
	  .result_format = TESS_RESULT_DATUM, \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE, \
	  .evaluate = compare_evaluate}, (code)}

#define INT4_ARITH(oid, code) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = TESS_RESULT_INT32, \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | \
	  TESS_FUNCTION_ANY_SHAPE, \
	  .evaluate = arith_evaluate}, (code)}

/* Unary minus: the one argument is the column. */
#define INT4_NEGATE(oid) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = TESS_RESULT_INT32, \
	  .flags = TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE, \
	  .evaluate = negate_evaluate}, TESS_ARITH_SUB}

static const Int4Function int4_functions[] = {
	INT4_COMPARE(F_INT4EQ, TESS_CMP_EQ),
	INT4_COMPARE(F_INT4NE, TESS_CMP_NE),
	INT4_COMPARE(F_INT4LT, TESS_CMP_LT),
	INT4_COMPARE(F_INT4LE, TESS_CMP_LE),
	INT4_COMPARE(F_INT4GT, TESS_CMP_GT),
	INT4_COMPARE(F_INT4GE, TESS_CMP_GE),
	INT4_ARITH(F_INT4PL, TESS_ARITH_ADD),
	INT4_ARITH(F_INT4MI, TESS_ARITH_SUB),
	INT4_ARITH(F_INT4MUL, TESS_ARITH_MUL),
	INT4_ARITH(F_INT4DIV, TESS_ARITH_DIV),
	INT4_ARITH(F_INT4MOD, TESS_ARITH_MOD),
	INT4_NEGATE(F_INT4UM),
};

/* The operation of the description a call names. */
static uint32
operation(const TessFunctionCall *call)
{
	const Int4Function *entry = (const Int4Function *)
		((const char *) call->function - offsetof(Int4Function, function));

	return entry->op;
}

/* Fail a call with an invalid argument before any kernel runs. */
static TessStatusCode
invalid(TessFunctionCall *call, const char *message)
{
	if (call->status != NULL && call->status->struct_size >= TESS_STATUS_MIN_SIZE)
	{
		call->status->code = TESS_ERROR_INVALID_ARGUMENT;
		strlcpy(call->status->sqlstate, "XX000", sizeof(call->status->sqlstate));
		strlcpy(call->status->message, message, sizeof(call->status->message));
	}
	return TESS_ERROR_INVALID_ARGUMENT;
}

static bool
valid_call(const TessFunctionCall *call)
{
	return call != NULL && call->struct_size >= TESS_FUNCTION_CALL_MIN_SIZE &&
		call->nargs == 2 && call->args != NULL &&
		call->args[0].struct_size >= TESS_FUNCTION_ARG_MIN_SIZE &&
		call->args[1].struct_size >= TESS_FUNCTION_ARG_MIN_SIZE;
}

/* column op scalar: the consumer puts the column first. */
static TessStatusCode
compare_evaluate(TessFunctionCall *call)
{
	if (!valid_call(call))
		return invalid(call, "an int4 comparison takes two arguments");
	if (call->args[0].column == NULL || call->args[1].column != NULL)
		return invalid(call, "an int4 comparison takes a column and a scalar");
	return tess_int4_filter(call->args[0].column, call->args[0].prepared,
							call->rows, (TessCompareOp) operation(call),
							DatumGetInt32(call->args[1].scalar), call->status);
}

/* Any shape but two scalars, which the consumer folds itself. */
static TessStatusCode
arith_evaluate(TessFunctionCall *call)
{
	const TessFunctionArg *left;
	const TessFunctionArg *right;
	TessArithOp op;

	if (!valid_call(call))
		return invalid(call, "int4 arithmetic takes two arguments");
	left = &call->args[0];
	right = &call->args[1];
	op = (TessArithOp) operation(call);
	if (left->column != NULL && right->column != NULL)
		return tess_int4_arith_columns(op, left->column, left->prepared,
									   right->column, right->prepared,
									   call->rows, (int32 *) call->values,
									   call->non_nulls, call->status);
	if (left->column != NULL)
		return tess_int4_arith_scalar(op, left->column,
									  DatumGetInt32(right->scalar),
									  left->prepared, call->rows,
									  (int32 *) call->values, call->non_nulls,
									  call->status);
	if (right->column != NULL)
		return tess_int4_arith_scalar_left(op, DatumGetInt32(left->scalar),
										   right->column, right->prepared,
										   call->rows, (int32 *) call->values,
										   call->non_nulls, call->status);
	return invalid(call, "int4 arithmetic needs a column argument");
}

/* -x is 0 - x, including the overflow of the smallest value. */
static TessStatusCode
negate_evaluate(TessFunctionCall *call)
{
	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs != 1 || call->args == NULL ||
		call->args[0].struct_size < TESS_FUNCTION_ARG_MIN_SIZE)
		return invalid(call, "int4 negation takes one argument");
	if (call->args[0].column == NULL)
		return invalid(call, "int4 negation takes a column");
	return tess_int4_arith_scalar_left(TESS_ARITH_SUB, 0, call->args[0].column,
									   call->args[0].prepared, call->rows,
									   (int32 *) call->values, call->non_nulls,
									   call->status);
}

void
_PG_init(void)
{
	const TessApi *api;
	void	  **rendezvous;
	int			i;

	rendezvous = find_rendezvous_variable(TESS_API_RENDEZVOUS);
	api = *rendezvous;
	if (api == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
				 errmsg("Tessera bridge must be loaded before tessera_kernels")));
	if (api->abi_version != TESS_API_ABI_VERSION ||
		api->struct_size < TESS_API_MIN_SIZE ||
		api->functions == NULL ||
		api->functions->abi_version != TESS_FUNCTION_REGISTRY_OPS_ABI_VERSION ||
		api->functions->struct_size < TESS_FUNCTION_REGISTRY_OPS_MIN_SIZE)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera function registry")));
	if (tess_kernels_abi_version() != TESS_KERNELS_ABI_VERSION)
		ereport(ERROR,
				(errcode(ERRCODE_FEATURE_NOT_SUPPORTED),
				 errmsg("incompatible Tessera kernels library")));
	for (i = 0; i < lengthof(int4_functions); i++)
		api->functions->add(&int4_functions[i].function);
}

/*
 * The float functions as batch functions: the comparisons of float8 and
 * float4 and between them, their arithmetic, negation and abs, and the
 * casts between floats and integers. This file registers them; the Rust
 * kernels compute them once a batch through tessera/float.h, with the
 * core's rules of utils/float.h: NaN is equal to itself and above every
 * number, overflow, underflow and division by zero fail with the core's
 * errors (22003 "value out of range: overflow" and "underflow", 22012) at
 * the row that meets them, a float becomes an integer rounded to even and
 * checked as dtoi4 checks it. A float4 compares with a float8 widened, as
 * float48eq does, which loses nothing.
 */
#include "postgres.h"

#include "fmgr.h"
#include "utils/fmgroids.h"

#include "tessera/bridge.h"
#include "tessera/float.h"

#include "internal.h"

/* A cast's operation: neither a comparison nor arithmetic. */
#define FLOAT_CAST (-1)

typedef struct FloatFunction
{
	TessFunction function;
	int			op;				/* TessCompareOp, TessFloatOp or FLOAT_CAST */
	TessFloatType left;
	TessFloatType right;
	TessFloatType result;
} FloatFunction;

static TessStatusCode float_compare_evaluate(TessFunctionCall *call);
static TessStatusCode float_value_evaluate(TessFunctionCall *call);

#define FLOAT_FLAGS \
	(TESS_FUNCTION_STRICT | TESS_FUNCTION_COLLATION_INSENSITIVE | TESS_FUNCTION_ANY_SHAPE)

#define FLOAT_COMPARE(oid, code, l, r) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_PREDICATE, \
	  .result_format = TESS_RESULT_DATUM, .flags = FLOAT_FLAGS, \
	  .evaluate = float_compare_evaluate}, (code), (l), (r), TESS_FLOAT_TYPE_FLOAT8}

#define FLOAT_VALUE(oid, code, l, r, res) \
	{{TESS_ABI_INITIALIZER(TESS_FUNCTION_ABI_VERSION, TessFunction), \
	  .funcid = (oid), .kind = TESS_FUNCTION_VALUE, \
	  .result_format = (res) == TESS_FLOAT_TYPE_INT4 || (res) == TESS_FLOAT_TYPE_INT2 ? TESS_RESULT_INT32 : TESS_RESULT_DATUM, \
	  .flags = FLOAT_FLAGS, \
	  .evaluate = float_value_evaluate}, (code), (l), (r), (res)}

#define FLOAT_COMPARES(eq, ne, lt, le, gt, ge, l, r) \
	FLOAT_COMPARE(eq, TESS_CMP_EQ, l, r), FLOAT_COMPARE(ne, TESS_CMP_NE, l, r), \
	FLOAT_COMPARE(lt, TESS_CMP_LT, l, r), FLOAT_COMPARE(le, TESS_CMP_LE, l, r), \
	FLOAT_COMPARE(gt, TESS_CMP_GT, l, r), FLOAT_COMPARE(ge, TESS_CMP_GE, l, r)

#define FLOAT_ARITHMETIC(pl, mi, mul, div, l, r, res) \
	FLOAT_VALUE(pl, TESS_FLOAT_ADD, l, r, res), FLOAT_VALUE(mi, TESS_FLOAT_SUB, l, r, res), \
	FLOAT_VALUE(mul, TESS_FLOAT_MUL, l, r, res), FLOAT_VALUE(div, TESS_FLOAT_DIV, l, r, res)

static const FloatFunction float_functions[] = {
	FLOAT_COMPARES(F_FLOAT8EQ, F_FLOAT8NE, F_FLOAT8LT, F_FLOAT8LE, F_FLOAT8GT, F_FLOAT8GE,
				   TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_COMPARES(F_FLOAT4EQ, F_FLOAT4NE, F_FLOAT4LT, F_FLOAT4LE, F_FLOAT4GT, F_FLOAT4GE,
				   TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4),
	FLOAT_COMPARES(F_FLOAT48EQ, F_FLOAT48NE, F_FLOAT48LT, F_FLOAT48LE, F_FLOAT48GT,
				   F_FLOAT48GE, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_COMPARES(F_FLOAT84EQ, F_FLOAT84NE, F_FLOAT84LT, F_FLOAT84LE, F_FLOAT84GT,
				   F_FLOAT84GE, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT4),
	FLOAT_ARITHMETIC(F_FLOAT8PL, F_FLOAT8MI, F_FLOAT8MUL, F_FLOAT8DIV,
					 TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_ARITHMETIC(F_FLOAT4PL, F_FLOAT4MI, F_FLOAT4MUL, F_FLOAT4DIV,
					 TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4),
	FLOAT_ARITHMETIC(F_FLOAT48PL, F_FLOAT48MI, F_FLOAT48MUL, F_FLOAT48DIV,
					 TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_ARITHMETIC(F_FLOAT84PL, F_FLOAT84MI, F_FLOAT84MUL, F_FLOAT84DIV,
					 TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_VALUE(F_FLOAT8UM, TESS_FLOAT_NEG, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_VALUE(F_FLOAT8ABS, TESS_FLOAT_ABS, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_VALUE(F_FLOAT4UM, TESS_FLOAT_NEG, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4),
	FLOAT_VALUE(F_FLOAT4ABS, TESS_FLOAT_ABS, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4),
	FLOAT_VALUE(F_FLOAT8_INT4, FLOAT_CAST, TESS_FLOAT_TYPE_INT4, TESS_FLOAT_TYPE_INT4, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_VALUE(F_FLOAT8_INT2, FLOAT_CAST, TESS_FLOAT_TYPE_INT4, TESS_FLOAT_TYPE_INT4, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_VALUE(F_FLOAT8_INT8, FLOAT_CAST, TESS_FLOAT_TYPE_INT8, TESS_FLOAT_TYPE_INT8, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_VALUE(F_FLOAT8_FLOAT4, FLOAT_CAST, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT8),
	FLOAT_VALUE(F_FLOAT4_INT4, FLOAT_CAST, TESS_FLOAT_TYPE_INT4, TESS_FLOAT_TYPE_INT4, TESS_FLOAT_TYPE_FLOAT4),
	FLOAT_VALUE(F_FLOAT4_INT2, FLOAT_CAST, TESS_FLOAT_TYPE_INT4, TESS_FLOAT_TYPE_INT4, TESS_FLOAT_TYPE_FLOAT4),
	FLOAT_VALUE(F_FLOAT4_INT8, FLOAT_CAST, TESS_FLOAT_TYPE_INT8, TESS_FLOAT_TYPE_INT8, TESS_FLOAT_TYPE_FLOAT4),
	FLOAT_VALUE(F_FLOAT4_FLOAT8, FLOAT_CAST, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT4),
	FLOAT_VALUE(F_INT4_FLOAT8, FLOAT_CAST, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_INT4),
	FLOAT_VALUE(F_INT2_FLOAT8, FLOAT_CAST, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_INT2),
	FLOAT_VALUE(F_INT8_FLOAT8, FLOAT_CAST, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_FLOAT8, TESS_FLOAT_TYPE_INT8),
	FLOAT_VALUE(F_INT4_FLOAT4, FLOAT_CAST, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_INT4),
	FLOAT_VALUE(F_INT2_FLOAT4, FLOAT_CAST, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_INT2),
	FLOAT_VALUE(F_INT8_FLOAT4, FLOAT_CAST, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_FLOAT4, TESS_FLOAT_TYPE_INT8),
};

static const FloatFunction *
float_function(const TessFunctionCall *call)
{
	return (const FloatFunction *) ((const char *) call->function -
									offsetof(FloatFunction, function));
}

static bool
float_call_valid(const TessFunctionCall *call, int nargs)
{
	if (call == NULL || call->struct_size < TESS_FUNCTION_CALL_MIN_SIZE ||
		call->nargs != nargs || call->args == NULL || call->rows == NULL)
		return false;
	for (int arg = 0; arg < nargs; arg++)
		if (call->args[arg].struct_size < TESS_FUNCTION_ARG_MIN_SIZE)
			return false;
	return true;
}

static inline TessFloatArg
float_arg(const TessFunctionArg *arg)
{
	return (TessFloatArg) {arg->column, arg->scalar};
}

/* Float comparisons of any shape, in the core's NaN-aware order. */
static TessStatusCode
float_compare_evaluate(TessFunctionCall *call)
{
	const FloatFunction *function;
	TessFloatArg left;
	TessFloatArg right;

	if (!float_call_valid(call, 2))
		return tess_call_invalid(call, "a float comparison takes two arguments");
	function = float_function(call);
	left = float_arg(&call->args[0]);
	right = float_arg(&call->args[1]);
	return tess_float_compare((TessCompareOp) function->op, function->left, &left,
							  function->right, &right, call->rows, call->status);
}

/* Float arithmetic, negation, abs and casts of any shape. */
static TessStatusCode
float_value_evaluate(TessFunctionCall *call)
{
	const FloatFunction *function;
	int			nargs;
	TessFloatArg left;
	TessFloatArg right;

	if (call == NULL || call->function == NULL)
		return tess_call_invalid(call, "a float function takes its arguments");
	function = float_function(call);
	nargs = function->op == FLOAT_CAST || function->op == TESS_FLOAT_NEG ||
		function->op == TESS_FLOAT_ABS ? 1 : 2;
	if (!float_call_valid(call, nargs) || call->values == NULL || call->non_nulls == NULL)
		return tess_call_invalid(call, "a float function takes its arguments");
	left = float_arg(&call->args[0]);
	if (function->op == FLOAT_CAST)
		return tess_float_cast(function->left, function->result, &left, call->rows,
							   call->values, call->non_nulls, call->status);
	right = nargs == 2 ? float_arg(&call->args[1]) : left;
	return tess_float_arith((TessFloatOp) function->op, function->left, &left,
							function->right, &right, call->rows, (Datum *) call->values,
							call->non_nulls, call->status);
}

/* Register the float functions. */
void
tess_register_float_functions(const TessFunctionRegistryOps *functions)
{
	for (int i = 0; i < lengthof(float_functions); i++)
		functions->add(&float_functions[i].function);
}

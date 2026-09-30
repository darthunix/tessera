/* Definitions shared within the tessera_kernels module. */
#ifndef TESSERA_KERNELS_INTERNAL_H
#define TESSERA_KERNELS_INTERNAL_H

#include "tessera/function.h"
#include "tessera/kernel_ops.h"
#include "tessera/row_mask.h"

/* The entry points of the linked Rust kernels, installed in the bridge. */
extern const TessKernelOps tess_kernel_ops;

/* The text, date, numeric and float functions (text.c, date.c, numeric.c, float.c), registered with the others. */
struct TessFunctionRegistryOps;
extern void tess_register_text_functions(const struct TessFunctionRegistryOps *functions);
extern void tess_register_date_functions(const struct TessFunctionRegistryOps *functions);
extern void tess_register_numeric_functions(const struct TessFunctionRegistryOps *functions);
extern void tess_register_float_functions(const struct TessFunctionRegistryOps *functions);

/* A numeric of an integer: a small one from the process's cache, else made in context. */
extern struct NumericData *tess_numeric_from_int64(int64 value, MemoryContext context);

/*
 * The numerics of the rows of write, each an int64 in values at its scale
 * in scales, replacing it: a small integer from the cache, the others
 * written by the kernels into one block of context; write is narrowed to
 * the rows the kernels wrote.
 */
extern TessStatusCode tess_numeric_results(MemoryContext context, Datum *values,
										   const uint8 *scales, TessRowMask *write,
										   TessStatus *status);

/*
 * Fail a call with code, sqlstate and message in its status, before any
 * kernel runs or for a check of the module's own after one; a NULL call or
 * a NULL or undersized status is left alone. Returns code.
 */
static inline TessStatusCode
tess_call_fail(TessFunctionCall *call, TessStatusCode code,
			   const char *sqlstate, const char *message)
{
	if (call != NULL && call->status != NULL &&
		call->status->struct_size >= TESS_STATUS_MIN_SIZE)
	{
		call->status->code = code;
		strlcpy(call->status->sqlstate, sqlstate, sizeof(call->status->sqlstate));
		strlcpy(call->status->message, message, sizeof(call->status->message));
	}
	return code;
}

/* Fail a call with an invalid argument, XX000. */
static inline TessStatusCode
tess_call_invalid(TessFunctionCall *call, const char *message)
{
	return tess_call_fail(call, TESS_ERROR_INVALID_ARGUMENT, "XX000", message);
}

/*
 * Scratch space of a call: local, a buffer of the caller's stack frame,
 * when size fits in it, else memory of the current context; release frees
 * only the latter.
 */
static inline void *
tess_scratch_alloc(Size size, void *local, Size local_size)
{
	return size <= local_size ? local : palloc(size);
}

static inline void
tess_scratch_release(void *pointer, void *local)
{
	if (pointer != local)
		pfree(pointer);
}

/* An empty mask of nrows rows over the local words, or allocated ones past them. */
static inline TessRowMask
tess_scratch_mask(uint64 *local, Size local_size, int nrows)
{
	int			nwords = tess_row_mask_word_count(nrows);
	uint64	   *words = tess_scratch_alloc(sizeof(uint64) * nwords, local, local_size);

	/* A batch is a word or a few: no call to memset. */
	for (int word = 0; word < nwords; word++)
		words[word] = 0;
	return (TessRowMask) {nrows, words};
}

#endif							/* TESSERA_KERNELS_INTERNAL_H */

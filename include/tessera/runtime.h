/* Runtime helpers for batch nodes, linked as libtessera_runtime.a. */
#ifndef TESSERA_RUNTIME_H
#define TESSERA_RUNTIME_H

#include "postgres.h"

#include "access/tupdesc.h"
#include "executor/tuptable.h"

#include "tessera/abi.h"
#include "tessera/batch.h"

/*
 * A builder collects rows from tuple slots into an owned column-major Datum
 * batch: one Datum and one NULL flag per row for each of the leading
 * ncolumns attributes, capacity rows at most. Pass-by-reference values are
 * copied into the builder's own memory context, so a slot may be reused
 * right after it was appended. The batch the builder returns exposes only
 * get_datum_column, with every row initialized (a NULL row holds 0), and
 * needs no release callback: reset reuses the storage. See docs/runtime.md.
 */
typedef struct TessBuilder TessBuilder;

typedef struct TessBuilderConfig
{
	Size		struct_size;
	/* Owns the builder and its arrays. */
	MemoryContext parent_context;
	/* Borrowed descriptor of the slots appended; outlives the builder. */
	TupleDesc	tuple_desc;
	/* Number of leading slot attributes copied into batch columns. */
	int			ncolumns;
	/* Rows in one batch; more than 64 is allowed. */
	int			capacity;
} TessBuilderConfig;

#define TESS_BUILDER_CONFIG_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessBuilderConfig, capacity)

/* Allocate an empty builder in the configured context. */
extern TessBuilder *tess_builder_create(const TessBuilderConfig *config);

/*
 * Discard copied values and start an empty batch. The caller must first
 * take a previously returned batch off its slot binding.
 */
extern void tess_builder_reset(TessBuilder *builder);

/* True after capacity rows were appended or the batch was finished. */
extern bool tess_builder_is_full(const TessBuilder *builder);

/*
 * Append one slot, materializing its leading ncolumns attributes; the slot
 * may be cleared or reused afterwards.
 */
extern void tess_builder_append_slot(TessBuilder *builder, TupleTableSlot *slot);

/*
 * Finish the batch and return it, or NULL without rows. The batch and its
 * columns stay valid until reset; finishing again returns the same batch.
 */
extern TessBatch *tess_builder_finish(TessBuilder *builder, Oid table_oid);

#endif							/* TESSERA_RUNTIME_H */

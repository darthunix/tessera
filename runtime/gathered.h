/*
 * The batch an input gathers the rows of its child's sparse batches into
 * (tess_input_set_gather): kept out of the installed headers, not part of
 * the C API.
 */
#ifndef TESSERA_RUNTIME_GATHERED_H
#define TESSERA_RUNTIME_GATHERED_H

#include "access/tupdesc.h"
#include "nodes/bitmapset.h"

#include "tessera/batch.h"
#include "tessera/layout.h"

typedef struct TessGathered TessGathered;

/*
 * A batch of capacity rows holding the request's columns (filter and
 * projection), typed by the child's result descriptor through its layout:
 * a column no target of the child maps to is an error. Allocated in
 * parent; the sets are copied.
 */
extern TessGathered *tess_gathered_create(MemoryContext parent, const TessLayout *layout,
										  TupleDesc desc, const Bitmapset *filter_columns,
										  const Bitmapset *projection_columns, int capacity);

/* The rows gathered so far, and its capacity. */
extern int	tess_gathered_rows(const TessGathered *gathered);
extern int	tess_gathered_capacity(const TessGathered *gathered);

/*
 * Copy the first selected rows of a child batch with stored values, as
 * many as fit, in order; those copied leave the child's mask when not all
 * fit. Returns the rows copied.
 */
extern int	tess_gathered_take(TessGathered *gathered, TessBatch *child);

/* The batch of the rows gathered, all selected; owned by the gathered. */
extern TessBatch *tess_gathered_batch(TessGathered *gathered);

/* Drop the rows and their values, for the next batch. */
extern void tess_gathered_reset(TessGathered *gathered);

#endif

/*
 * What the runtime's own modules ask of a heap batch, kept out of the
 * installed headers: not part of the C API.
 */
#ifndef TESSERA_RUNTIME_HEAP_BATCH_INTERNAL_H
#define TESSERA_RUNTIME_HEAP_BATCH_INTERNAL_H

#include "tessera/batch.h"

/*
 * Whether batch is a heap batch, whose columns are the attributes of its
 * rows, deformed on request without computing anything.
 */
extern bool tess_batch_is_heap(const TessBatch *batch);

#endif

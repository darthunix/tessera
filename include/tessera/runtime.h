/*
 * Runtime helpers for batch nodes, linked as libtessera_runtime.a: every
 * part's header, for a node that uses many of them; a part's own header
 * (tessera/runtime_*.h) is enough for one.
 */
#ifndef TESSERA_RUNTIME_H
#define TESSERA_RUNTIME_H

#include "tessera/runtime_api.h"
#include "tessera/runtime_builder.h"
#include "tessera/runtime_heap_batch.h"
#include "tessera/runtime_input.h"
#include "tessera/runtime_output.h"
#include "tessera/runtime_project.h"
#include "tessera/runtime_qual.h"
#include "tessera/runtime_rows.h"
#include "tessera/runtime_shared_stats.h"
#include "tessera/runtime_spill.h"
#include "tessera/runtime_unary.h"

#endif							/* TESSERA_RUNTIME_H */

/*
 * The Rust kernels as a table of operations, for modules that do not link
 * them.
 *
 * Only the tessera_kernels module links the static library of the kernels.
 * It publishes the entry points other modules need, such as the key hashes
 * and the hash table a join or grouping node keeps its state in, as a
 * TessKernelOps through the bridge's kernel registry; a node reaches them
 * through tess_runtime_kernels() and calls them by pointer. Every
 * operation has the prototype, arguments and contract of the entry point
 * named in its comment (tessera/kernels.h, tessera/table.h). See
 * docs/bridge.md.
 */
#ifndef TESSERA_KERNEL_OPS_H
#define TESSERA_KERNEL_OPS_H

#include "postgres.h"

#include "tessera/abi.h"
#include "tessera/kernels.h"
#include "tessera/table.h"

#define TESS_KERNEL_OPS_ABI_VERSION 0
#define TESS_KERNEL_REGISTRY_OPS_ABI_VERSION 0

/*
 * The operations of one build of the kernels. The provider owns the table
 * and keeps it valid and unchanged from installation until it clears it.
 */
typedef struct TessKernelOps
{
	uint32		abi_version;
	Size		struct_size;
	/* TESS_TABLE_FORMAT_VERSION of the kernels' build. */
	uint32		table_format_version;

	/* tess_int4_hash */
	TessStatusCode (*int4_hash) (const TessDatumColumn *column,
								 const TessRowMask *prepared,
								 const TessRowMask *rows,
								 TessNullKeys nulls,
								 uint32 *hashes,
								 TessRowMask *valid,
								 TessStatus *status);
	/* tess_int4_hash_next */
	TessStatusCode (*int4_hash_next) (const TessDatumColumn *column,
									  const TessRowMask *prepared,
									  TessNullKeys nulls,
									  uint32 *hashes,
									  TessRowMask *valid,
									  TessStatus *status);
	/* tess_int8_hash */
	TessStatusCode (*int8_hash) (const TessDatumColumn *column,
								 const TessRowMask *prepared,
								 const TessRowMask *rows,
								 TessNullKeys nulls,
								 uint32 *hashes,
								 TessRowMask *valid,
								 TessStatus *status);
	/* tess_int8_hash_next */
	TessStatusCode (*int8_hash_next) (const TessDatumColumn *column,
									  const TessRowMask *prepared,
									  TessNullKeys nulls,
									  uint32 *hashes,
									  TessRowMask *valid,
									  TessStatus *status);

	/* tess_table_size */
	TessStatusCode (*table_size) (int nkeys,
								  const TessTableKeyKind *kinds,
								  Size payload_size,
								  uint64 capacity,
								  Size *size,
								  TessStatus *status);
	/* tess_table_create */
	TessStatusCode (*table_create) (void *region,
									Size len,
									int nkeys,
									const TessTableKeyKind *kinds,
									Size payload_size,
									uint64 capacity,
									TessStatus *status);
	/* tess_table_stats */
	TessStatusCode (*table_stats) (const void *region,
								   Size len,
								   TessTableStats *stats,
								   TessStatus *status);
	/* tess_table_insert */
	TessStatusCode (*table_insert) (void *region,
									Size len,
									const uint32 *hashes,
									int nkeys,
									const TessTableKey *keys,
									const uint8 *payload,
									TessRowMask *pending,
									uint32 *offsets,
									TessStatus *status);
	/* tess_table_probe */
	TessStatusCode (*table_probe) (const void *region,
								   Size len,
								   const uint32 *hashes,
								   int nkeys,
								   const TessTableKey *keys,
								   const TessRowMask *rows,
								   uint32 *matches,
								   TessRowMask *found,
								   TessStatus *status);
	/* tess_table_next_match */
	TessStatusCode (*table_next_match) (const void *region,
										Size len,
										uint32 *offsets,
										const TessRowMask *rows,
										TessRowMask *found,
										TessStatus *status);
	/* tess_table_gather */
	TessStatusCode (*table_gather) (const void *region,
									Size len,
									const uint32 *offsets,
									const TessRowMask *rows,
									Size at,
									Datum *values,
									TessStatus *status);
	/* tess_table_record */
	TessStatusCode (*table_record) (const void *region,
									Size len,
									uint32 offset,
									TessTableRecord *record,
									TessStatus *status);
	/* tess_table_find_or_insert */
	TessStatusCode (*table_find_or_insert) (void *region,
											Size len,
											const uint32 *hashes,
											int nkeys,
											const TessTableKey *keys,
											TessRowMask *pending,
											uint32 *offsets,
											TessRowMask *inserted,
											TessStatus *status);
	/* tess_table_payload */
	TessStatusCode (*table_payload) (void *region,
									 Size len,
									 uint32 offset,
									 uint8 **payload,
									 TessStatus *status);
	/* tess_table_scan */
	TessStatusCode (*table_scan) (void *region,
								  Size len,
								  uint64 *cursor,
								  uint32 *offsets,
								  int capacity,
								  int *count,
								  TessStatus *status);
	/* tess_table_grow */
	TessStatusCode (*table_grow) (void *region,
								  Size len,
								  TessStatus *status);
	/* tess_table_insert_grouped */
	TessStatusCode (*table_insert_grouped) (void *region,
											Size len,
											const uint32 *hashes,
											int nkeys,
											const TessTableKey *keys,
											const uint8 *payload,
											TessRowMask *pending,
											uint32 *offsets,
											TessRowMask *duplicates,
											TessStatus *status);
	/* tess_table_next_in_group */
	TessStatusCode (*table_next_in_group) (const void *region,
										   Size len,
										   uint32 *offsets,
										   const TessRowMask *rows,
										   TessRowMask *found,
										   TessStatus *status);
} TessKernelOps;

#define TESS_KERNEL_OPS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessKernelOps, table_next_in_group)

/*
 * The registry of the kernels: one table per backend, installed by the
 * module that links them.
 */
typedef struct TessKernelRegistryOps
{
	uint32		abi_version;
	Size		struct_size;
	/*
	 * Install without copying. Installing the same table again is safe;
	 * another table while one is installed is an error, as is an invalid
	 * table (a wrong version, a short size, a missing operation).
	 */
	void		(*set) (const TessKernelOps *ops);
	/*
	 * Remove this exact table if it is installed, without freeing it. NULL
	 * and a table that is not installed are ignored.
	 */
	void		(*clear) (const TessKernelOps *ops);
	/*
	 * The installed table, or NULL. The borrowed pointer is valid until the
	 * provider clears it; the consumer must not modify it.
	 */
	const TessKernelOps *(*get) (void);
} TessKernelRegistryOps;

#define TESS_KERNEL_REGISTRY_OPS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessKernelRegistryOps, get)

#endif							/* TESSERA_KERNEL_OPS_H */

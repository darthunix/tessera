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
#include "tessera/spill.h"
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
	TessStatusCode (*table_create) (void *index,
									Size len,
									int nkeys,
									const TessTableKeyKind *kinds,
									Size payload_size,
									uint64 capacity,
									TessStatus *status);
	/* tess_table_stats */
	TessStatusCode (*table_stats) (const TessTableRef *table,
								   TessTableStats *stats,
								   TessStatus *status);
	/* tess_table_chunk_init */
	TessStatusCode (*table_chunk_init) (void *base, Size len,
										TessStatus *status);
	/* tess_table_append */
	TessStatusCode (*table_append) (const TessTableRef *table,
									int chunk,
									Size payload_size,
									const uint32 *hashes,
									int nkeys,
									const TessTableKey *keys,
									const uint8 *payload,
									TessRowMask *pending,
									uint32 *offsets,
									TessStatus *status);
	/* tess_table_link */
	TessStatusCode (*table_link) (const TessTableRef *table,
								  int chunk,
								  Size *from,
								  uint64 *linked,
								  uint64 *duplicates,
								  TessStatus *status);
	/* tess_table_link_grouped */
	TessStatusCode (*table_link_grouped) (const TessTableRef *table,
										  int chunk,
										  Size *from,
										  uint64 *linked,
										  uint64 *duplicates,
										  TessStatus *status);
	/* tess_table_probe */
	TessStatusCode (*table_probe) (const TessTableRef *table,
								   const uint32 *hashes,
								   int nkeys,
								   const TessTableKey *keys,
								   const TessRowMask *rows,
								   uint32 *matches,
								   TessRowMask *found,
								   TessStatus *status);
	/* tess_table_next_match */
	TessStatusCode (*table_next_match) (const TessTableRef *table,
										uint32 *offsets,
										const TessRowMask *rows,
										TessRowMask *found,
										TessStatus *status);
	/* tess_table_gather */
	TessStatusCode (*table_gather) (const TessTableRef *table,
									const uint32 *offsets,
									const TessRowMask *rows,
									Size at,
									Datum *values,
									TessStatus *status);
	/* tess_table_record */
	TessStatusCode (*table_record) (const TessTableRef *table,
									uint32 offset,
									TessTableRecord *record,
									TessStatus *status);
	/* tess_table_find_or_insert */
	TessStatusCode (*table_find_or_insert) (const TessTableRef *table,
											int chunk,
											const uint32 *hashes,
											int nkeys,
											const TessTableKey *keys,
											TessRowMask *pending,
											uint32 *offsets,
											TessRowMask *inserted,
											TessStatus *status);
	/* tess_table_payload */
	TessStatusCode (*table_payload) (const TessTableRef *table,
									 uint32 offset,
									 uint8 **payload,
									 TessStatus *status);
	/* tess_table_scan */
	TessStatusCode (*table_scan) (const TessTableRef *table,
								  uint64 *cursor,
								  uint32 *offsets,
								  int capacity,
								  int *count,
								  TessStatus *status);
	/* tess_table_regrow */
	TessStatusCode (*table_regrow) (const TessTableRef *table,
									void *index,
									Size len,
									uint64 capacity,
									TessStatus *status);
	/* tess_table_next_in_group */
	TessStatusCode (*table_next_in_group) (const TessTableRef *table,
										   uint32 *offsets,
										   const TessRowMask *rows,
										   TessRowMask *found,
										   TessStatus *status);
	/* tess_table_accumulate */
	TessStatusCode (*table_accumulate) (const TessTableRef *table,
										const uint32 *offsets,
										const TessRowMask *rows,
										TessTableAccumulate op,
										const TessDatumColumn *column,
										const TessRowMask *prepared,
										Size value_at,
										Size flags_at,
										uint32 flag_bit,
										TessStatus *status);
	/* tess_table_gather_key */
	TessStatusCode (*table_gather_key) (const TessTableRef *table,
										const uint32 *offsets,
										const TessRowMask *rows,
										int key,
										Datum *values,
										bool *isnull,
										TessStatus *status);
	/* tess_table_bloom_words */
	TessStatusCode (*table_bloom_words) (uint64 records, Size *nwords,
										 TessStatus *status);
	/* tess_table_bloom */
	TessStatusCode (*table_bloom) (const TessTableRef *table,
								   uint64 *words, Size nwords,
								   TessStatus *status);
	/* tess_bloom_probe */
	TessStatusCode (*bloom_probe) (const uint64 *words, Size nwords,
								   const uint32 *hashes,
								   const TessRowMask *rows,
								   TessRowMask *found,
								   TessStatus *status);
	/* tess_bloom_shared_words */
	TessStatusCode (*bloom_shared_words) (uint64 records, Size *nwords,
										  TessStatus *status);
	/* tess_bloom_shared_init */
	TessStatusCode (*bloom_shared_init) (uint64 *words, Size nwords,
										 TessStatus *status);
	/* tess_table_try_build_bloom */
	TessStatusCode (*table_try_build_bloom) (const TessTableRef *table,
											 uint64 *words, Size nwords,
											 bool *built,
											 TessStatus *status);
	/* tess_bloom_shared_ready */
	TessStatusCode (*bloom_shared_ready) (uint64 *words, Size nwords,
										  bool *ready,
										  TessStatus *status);
	/* tess_bloom_shared_probe */
	TessStatusCode (*bloom_shared_probe) (uint64 *words, Size nwords,
										  const uint32 *hashes,
										  const TessRowMask *rows,
										  TessRowMask *found,
										  TessStatus *status);
	/* tess_build_counters_init */
	TessStatusCode (*build_counters_init) (uint64 *counters,
										   TessStatus *status);
	/* tess_build_report */
	TessStatusCode (*build_report) (uint64 *counters, uint64 records,
									uint64 null_columns, TessStatus *status);
	/* tess_build_take_chunk */
	TessStatusCode (*build_take_chunk) (uint64 *counters, uint64 *number,
										TessStatus *status);
	/* tess_build_add_duplicates */
	TessStatusCode (*build_add_duplicates) (uint64 *counters, uint64 duplicates,
											TessStatus *status);
	/* tess_build_totals */
	TessStatusCode (*build_totals) (uint64 *counters, uint64 *records,
									uint64 *null_columns, uint64 *chunks,
									uint64 *duplicates, TessStatus *status);
	/* tess_build_step */
	TessStatusCode (*build_step) (TessBuildParticipant *participant,
								  uint64 *counters, uint32 reply,
								  uint32 *action, TessStatus *status);
	/* tess_table_fingerprint */
	TessStatusCode (*table_fingerprint) (const TessTableRef *table,
										 uint64 *fingerprint,
										 TessStatus *status);
	/* tess_spill_header_write */
	TessStatusCode (*spill_header_write) (void *out, Size len,
										  const TessSpillHeader *header,
										  uint64 max_len, TessStatus *status);
	/* tess_spill_header_read */
	TessStatusCode (*spill_header_read) (const void *bytes, Size len,
										 uint64 fingerprint, uint64 max_len,
										 TessSpillHeader *header,
										 TessStatus *status);
	/* tess_table_append_partitioned */
	TessStatusCode (*table_append_partitioned) (const TessTableRef *table,
												const uint32 *partition_chunks,
												int npartitions, uint32 shift,
												Size payload_size,
												const uint32 *hashes, int nkeys,
												const TessTableKey *keys,
												const uint8 *payload,
												TessRowMask *pending,
												uint32 *offsets,
												TessStatus *status);
	/* tess_table_split */
	TessStatusCode (*table_split) (const TessTableRef *table, int nkeys,
								   const TessTableKeyKind *kinds,
								   Size payload_size,
								   const uint32 *partition_chunks,
								   int npartitions, uint32 shift, int source,
								   Size *from, int capacity, uint32 *offsets,
								   uint32 *hashes, int *count, int *full,
								   TessStatus *status);
	/* tess_bloom_add */
	TessStatusCode (*bloom_add) (uint64 *words, Size nwords,
								 const uint32 *hashes, const TessRowMask *rows,
								 TessStatus *status);
	/* tess_table_find_or_insert_partitioned */
	TessStatusCode (*table_find_or_insert_partitioned) (const TessTableRef *table,
														const uint32 *partition_chunks,
														int npartitions,
														uint32 shift,
														const uint32 *hashes,
														int nkeys,
														const TessTableKey *keys,
														TessRowMask *pending,
														uint32 *offsets,
														TessRowMask *inserted,
														TessStatus *status);
	/* tess_table_combine */
	TessStatusCode (*table_combine) (const TessTableRef *table, int source,
									 Size *from, int chunk, int naggregates,
									 const TessTableCombine *combines,
									 int *merged, int *stop,
									 TessStatus *status);
	/* tess_table_spill_words */
	TessStatusCode (*table_spill_words) (int capacity,
										 Size *nwords,
										 TessStatus *status);
	/* tess_table_spill_init */
	TessStatusCode (*table_spill_init) (uint64 *words,
										Size nwords,
										uint64 budget,
										TessStatus *status);
	/* tess_table_spill_split */
	TessStatusCode (*table_spill_split) (uint64 *words,
										 Size nwords,
										 uint32 partitions,
										 uint32 *in_force,
										 TessStatus *status);
	/* tess_table_spill_partitions */
	TessStatusCode (*table_spill_partitions) (uint64 *words,
											  Size nwords,
											  uint32 *partitions,
											  TessStatus *status);
	/* tess_table_spill_add_bytes */
	TessStatusCode (*table_spill_add_bytes) (uint64 *words,
											 Size nwords,
											 int64 delta,
											 int32 partition,
											 bool *over,
											 TessStatus *status);
	/* tess_table_spill_evict */
	TessStatusCode (*table_spill_evict) (uint64 *words,
										 Size nwords,
										 int32 *partition,
										 TessStatus *status);
	/* tess_table_spill_flags */
	TessStatusCode (*table_spill_flags) (uint64 *words,
										 Size nwords,
										 uint32 partition,
										 bool *on_disk,
										 bool *alone,
										 TessStatus *status);
	/* tess_table_spill_records */
	TessStatusCode (*table_spill_records) (uint64 *words,
										   Size nwords,
										   uint32 partition,
										   uint64 added,
										   uint64 *records,
										   TessStatus *status);
	/* tess_table_spill_start */
	TessStatusCode (*table_spill_start) (uint64 *words,
										 Size nwords,
										 uint32 *partition,
										 TessStatus *status);
	/* tess_table_spill_take_file */
	TessStatusCode (*table_spill_take_file) (uint64 *words,
											 Size nwords,
											 uint32 partition,
											 bool outer,
											 uint32 *file,
											 TessStatus *status);
	/* tess_table_spill_take_alone */
	TessStatusCode (*table_spill_take_alone) (uint64 *words,
											  Size nwords,
											  uint32 partition,
											  bool *taken,
											  TessStatus *status);
	/* tess_round_step */
	TessStatusCode (*round_step) (TessBuildParticipant *participant,
								  uint32 reply,
								  uint32 *action,
								  TessStatus *status);
	/* tess_bloom_shared_add */
	TessStatusCode (*bloom_shared_add) (uint64 *words,
										Size nwords,
										const uint32 *hashes,
										const TessRowMask *rows,
										TessStatus *status);
	/* tess_table_spill_evictions */
	TessStatusCode (*table_spill_evictions) (uint64 *words,
											 Size nwords,
											 uint64 *evictions,
											 TessStatus *status);
	/* tess_spill_pack */
	TessStatusCode (*spill_pack) (const void *chunk, Size len, void *out,
								  Size capacity, Size *packed,
								  TessStatus *status);
	/* tess_spill_unpack */
	TessStatusCode (*spill_unpack) (const void *packed, Size len, void *chunk,
									Size chunk_len, TessStatus *status);
	/* tess_table_payloads */
	TessStatusCode (*table_payloads) (const TessTableRef *table,
									  const uint32 *offsets,
									  const TessRowMask *rows,
									  uint8 **payloads,
									  TessStatus *status);
} TessKernelOps;

#define TESS_KERNEL_OPS_MIN_SIZE \
	TESS_ABI_SIZE_INCLUDING_FIELD(TessKernelOps, table_payloads)

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

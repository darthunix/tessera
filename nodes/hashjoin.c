#include "postgres.h"

#include "access/parallel.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "nodes/makefuncs.h"
#include "nodes/nodeFuncs.h"
#include "optimizer/optimizer.h"
#include "utils/datum.h"
#include "utils/expandeddatum.h"
#include "utils/lsyscache.h"
#include "utils/memutils.h"
#include "storage/barrier.h"
#include "storage/shm_toc.h"
#include "utils/dsa.h"
#include "utils/wait_event.h"
#include "utils/ruleutils.h"

#include "tessera/kernel_ops.h"
#include "tessera/plan.h"
#include "tessera/runtime.h"

#include "internal.h"

/*
 * TessHashJoin joins two batch children on equalities of integer keys.
 * It builds the rows of the inner child into the hash table of the Rust
 * kernels (tessera/table.h), which it reaches through the bridge's kernel
 * registry, and probes the table with the batches of the outer child. A
 * batch it publishes has the physical rows of an outer batch and selects
 * the rows that found a record: the outer columns are the outer batch's
 * own, the inner columns come from the records' payload when a parent asks
 * for them. A key held by several inner rows has as many records, and the
 * outer batch is published once per record (a round). See docs/nodes.md.
 */

/* The payload holds a word of NULL bits, so at most 64 inner columns. */
#define JOIN_MAX_PAYLOAD 64
/* Rows of the buffers before the first batch shows its size. */
#define JOIN_INITIAL_ROWS 64
/* Rows of a compact batch: the pairs of several rounds, one after another. */
#define JOIN_COMPACT_ROWS 64
/* A round with at least this many rows is published as it is, not copied. */
#define JOIN_DENSE_ROUND 32
/*
 * The Bloom filter: after this many probed rows with a valid key, a table
 * of at least JOIN_BLOOM_MIN_ROWS rows gets one when fewer than half of
 * them found a record. A smaller table stays in the cache, where a miss
 * costs less than the check.
 */
#define JOIN_BLOOM_SAMPLE 4096
#define JOIN_BLOOM_MIN_ROWS 4096

/* The counters every participant of a parallel plan shares. */
enum
{
	JOIN_BUILDS,
	JOIN_BUILD_ROWS,
	/* Summed over the builds; EXPLAIN shows the mean. */
	JOIN_BUCKETS,
	/* The chunks of the records, summed over the builds. */
	JOIN_CHUNKS,
	JOIN_PROBE_ROWS,
	JOIN_MATCHES,
	/* The participant's peak bytes, and what of them exceeded hash_mem. */
	JOIN_MEMORY,
	JOIN_OVERRUN,
	/* Batches of pairs copied one after another: compact mode. */
	JOIN_COMPACT_BATCHES,
	/* Pairs the residual join clauses removed. */
	JOIN_FILTER_REMOVED,
	JOIN_OUTPUT_REMOVED,
	/* Bloom filters built, and the valid probe rows they rejected. */
	JOIN_BLOOM_FILTERS,
	JOIN_BLOOM_REMOVED,
	/* Filters the outer child took, to check its rows with. */
	JOIN_BLOOM_BELOW,
	/*
	 * Spilling: the most partitions of a build, the resident ones summed
	 * over the builds, the blocks and bytes written, and the partitions
	 * whose tail was joined as it was, never written.
	 */
	JOIN_BATCHES,
	JOIN_RESIDENT,
	JOIN_SPILLED,
	JOIN_DISK,
	JOIN_TAILS,
	/* Passes over a partition's outer rows past the first, for one too large. */
	JOIN_PASSES,
	JOIN_NCOUNTERS
};

/*
 * The table's records lie in chunks the node allocates: the first of a
 * table, or of a participant of a shared build, of JOIN_FIRST_CHUNK
 * bytes, so that a small inner side takes little, and the others of the
 * most a chunk may have. Records never move; the index is made once the
 * inner side is read, for exactly its rows.
 */
#define JOIN_FIRST_CHUNK (64 * 1024)
#define JOIN_CHUNK_LEN TESS_TABLE_MAX_CHUNK_LEN

/*
 * A shared build (plan data "shared"): one table in the query's dynamic
 * shared memory, in phases the build barrier separates (see
 * tessera/table.h, TESS_BUILD_*). Every participant appends its share of
 * the inner side to chunks of its own, each after a header that enters
 * it in the table's list under the number the build counters gave it;
 * the elected one then makes the index and the directory of the chunks
 * by number, and every participant links its own chunks.
 */
typedef struct JoinChunk
{
	dsa_pointer next;
	uint64		number;
	Size		len;
} JoinChunk;

#define JOIN_CHUNK_HEADER MAXALIGN(sizeof(JoinChunk))

/*
 * By-reference inner values live in chunks of their own, the first of
 * JOIN_VALUE_FIRST bytes and the others of JOIN_VALUE_CHUNK, a value
 * larger than a quarter of one getting a chunk of its own: in the node's
 * memory for a table of its own, in the query's dynamic shared memory for
 * a shared one, where each participant fills chunks of its own, numbered
 * under the lock and entered in the table's list after a JoinChunk
 * header. A payload word refers to a value by its chunk's number plus one
 * and its byte in the chunk (JOIN_VALUE_REF), 0 standing for NULL: no
 * address of a process, so that the words mean the same in every process
 * and on disk. A gather turns them into addresses.
 */
#define JOIN_VALUE_FIRST (64 * 1024)
#define JOIN_VALUE_CHUNK (1024 * 1024)
#define JOIN_VALUE_REF(number, byte) ((((uint64) (number) + 1) << 32) | (uint64) (byte))

/* The part of the node's DSM chunk a shared build uses, before the counters. */
typedef struct JoinShared
{
	Barrier		build;
	/*
	 * The index, and the directory of the chunks: nchunks dsa_pointers to
	 * their bases by number, then their lengths; set by the elected
	 * participant.
	 */
	dsa_pointer index;
	Size		index_len;
	dsa_pointer directory;
	int			nchunks;
	/* The records appended, the NULL columns and the chunks numbered. */
	uint64		counters[TESS_BUILD_COUNTER_WORDS];
	/* The shared Bloom filter, sized with the table; one participant builds it. */
	dsa_pointer filter;
	Size		filter_words;
	/*
	 * Every chunk of records and every chunk of values, under the lock,
	 * which also numbers the value chunks; the elected participant makes
	 * the value chunks' directory as it does the records'.
	 */
	slock_t		lock;
	dsa_pointer chunks;
	dsa_pointer values;
	uint32		next_value_chunk;
	dsa_pointer value_directory;
	int			nvalue_chunks;
} JoinShared;

/*
 * Spilling (plan item 5.6, docs/spill.md). A serial table that outgrows
 * hash_mem keeps its records in partitions by the hash's low bits: the
 * resident ones whole in memory, the others with only their last chunk
 * (the tail) in memory and their full chunks written to temporary files.
 * The outer rows of those partitions are written as records too, of a
 * table of the outer keys whose payload is the outer columns the node
 * needs, and each partition is joined once the outer child is done. A
 * partition's chunks are small, so that every partition's tail fits:
 * about hash_mem / (16 partitions), from JOIN_SPILL_MIN_CHUNK to 1 MB.
 */
#define JOIN_SPILL_MIN_CHUNK (8 * 1024)
#define JOIN_SPILL_MIN_PARTITIONS 4
#define JOIN_SPILL_MAX_PARTITIONS 1024

/*
 * A partition of one side: its record chunks in memory (every one of a
 * resident partition, the tail of another) and the value chunks in memory
 * that are not written yet, whose values its records refer to.
 */
typedef struct SpillPart
{
	int		   *chunks;
	int			nchunks;
	int			chunk_slots;
	int		   *values;
	int			nvalues;
	int			value_slots;
	/* The value chunk values are copied into, -1 for none, and its room. */
	int			value_current;
	Size		value_len;
	Size		value_used;
	/* Bytes of the value chunks in memory, and of everything in memory. */
	Size		value_bytes;
	Size		bytes;
	uint64		rows;
	/* Bytes of the blocks written. */
	uint64		disk_bytes;
	bool		resident;
	/* A block of the partition went to disk. */
	bool		written;
	/* The partition's values outgrew a chunk: it is written after the batch. */
	bool		queued;
	/* The last batch that saw a row of it pending, to visit it once. */
	uint64		stamp;
} SpillPart;

/*
 * One side's records by partition: the inner rows while building, the
 * outer rows of the partitions on disk while probing. The records are in
 * chunks of the table's format, a partition appending to chunk
 * current[partition]; chunk 0 is empty, with no room, for a partition
 * with no chunk yet. The by-reference values are in value chunks numbered
 * for the side, which a payload word refers to as a table's does.
 */
typedef struct SpillSide
{
	MemoryContext context;
	int			nkeys;
	TessTableKeyKind kinds[TESS_TABLE_MAX_KEYS];
	/* The payload: a word of NULL bits, then nwords words. */
	int			nwords;
	int16	   *typlens;
	bool	   *byvals;
	Size		payload_size;
	Size		chunk_len;
	/* The chunks, and a table over them with an index of the layout only. */
	void	  **bases;
	Size	   *lens;
	int			nchunks;
	int			slots;
	TessTableRef ref;
	uint32	   *current;
	/* The value chunks by number: base (NULL when not in memory), bytes used and allocated. */
	char	  **value_bases;
	Size	   *value_lens;
	Size	   *value_allocated;
	int			nvalues;
	int			value_slots;
	int			npartitions;
	SpillPart  *parts;
	TessSpill  *file;
	uint64		fingerprint;
	uint32		next_number;
	Size		bytes;
	/* Partitions whose values outgrew a chunk, written after the batch. */
	int		   *queue;
	int			nqueue;
} SpillSide;

typedef struct JoinSpill
{
	MemoryContext context;
	int			npartitions;
	uint32		shift;
	SpillSide	build;
	SpillSide	probe;
	/* A Bloom filter of every inner row, checked before an outer row is written. */
	uint64	   *bloom;
	Size		bloom_words;
	uint64		total_rows;
	uint64		stamp;
	/* The outer child is done: the partitions on disk are joined in turn. */
	bool		joining;
	/*
	 * A compact batch holds pairs of the joined partition's table: the next
	 * partition waits until it goes out.
	 */
	bool		holding;
	/* The outer child returned its last batch. */
	bool		child_done;
	/*
	 * A partition too large for memory is joined in pieces, each a run of
	 * whole groups of its file (values, then the chunks that refer to
	 * them), and its outer rows read once per piece: the file's reader,
	 * whether the pieces are done, a group's first value chunk read ahead
	 * for the next piece, and, for left, semi and anti joins, a bit per
	 * outer row of the partition, in the order read, set once it has a
	 * pair; a last pass without a table answers left and anti joins' rows
	 * without one. Each batch of outer rows starts at an ordinal.
	 */
	bool		multipass;
	TessSpillReader *build_reader;
	bool		pieces_done;
	bool		final_pass;
	void	   *carried;
	TessSpillHeader carried_header;
	uint64	   *matched_rows;
	uint64		ordinal;
	uint64		batch_ordinal;
	int			partition;
	/* The joined partition's chunks and values read back. */
	MemoryContext part_context;
	int		   *loaded_values;
	int			nloaded;
	int			loaded_slots;
	/* The value chunks read back before the outer chunk being read. */
	int		   *block_values;
	int			nblock_values;
	int			block_slots;
	/*
	 * The outer rows of the joined partition: the file's reader, the chunk
	 * being read and the values before it, the walk's cursor in it, and the
	 * tail once the file is done.
	 */
	TessSpillReader *reader;
	MemoryContext block_context;
	void	   *block;
	Size		block_len;
	bool		tail_read;
	uint64		cursor;
	TessTableRef block_ref;
	void	   *block_base[1];
	Size		block_lens[1];
	/* The batch of outer rows read back: each stored column's values. */
	TessBatch	batch;
	uint32		offsets[JOIN_COMPACT_ROWS];
	uint64		bits[1];
	uint64		words[JOIN_COMPACT_ROWS];
	Datum	  **values;
	bool	  **isnull;
	/* The outer child's column of each stored word, and the word of each child column plus one. */
	int		   *stored;
	int		   *word_of;
	int			nchild;
	/*
	 * Appending a batch's rows to a side: the inner child's column of each
	 * payload word of the inner side, the payload of one batch, the rows
	 * pending before an append, and the columns read.
	 */
	int		   *build_children;
	uint64	   *payload;
	int			payload_rows;
	uint64	   *before_bits;
	TessDatumColumn *columns;
} JoinSpill;

/* Which child a column of the scan tuple comes from. */
typedef enum JoinSide
{
	JOIN_SIDE_OUTER = 0,
	JOIN_SIDE_INNER = 1
} JoinSide;

typedef struct TessHashJoinState
{
	CustomScanState css;
	const TessKernelOps *kernels;
	PlanState  *outer;
	PlanState  *inner;
	TessInput  *outer_input;
	TessInput  *inner_input;
	TessOutput *output;
	/* The node's layout: which scan tuple column each target is. */
	TessLayout	layout;
	/* The parent's request, frozen at the first execution; NULL before. */
	const TessRequest *request;

	/* Per scan tuple column: its side and its column in that child's batches. */
	int			ncolumns;
	int		   *sides;
	int		   *child_columns;
	/* Per scan tuple column: 1 + its payload word, or 0 when not kept. */
	int		   *payload_words;
	/* The key's column in each child's batches, and its kind there. */
	/* The keys: each one's column in each child's batches, and its kind there. */
	int			nkeys;
	int			outer_keys[TESS_TABLE_MAX_KEYS];
	int			inner_keys[TESS_TABLE_MAX_KEYS];
	TessTableKeyKind outer_kinds[TESS_TABLE_MAX_KEYS];
	TessTableKeyKind inner_kinds[TESS_TABLE_MAX_KEYS];
	/* The key columns of the batch being inserted or probed. */
	TessDatumColumn key_columns[TESS_TABLE_MAX_KEYS];
	TessTableKey table_keys[TESS_TABLE_MAX_KEYS];
	/* Every outer row matches at most one inner row: no second round. */
	bool		inner_unique;
	/*
	 * INNER, SEMI, ANTI or LEFT: the kinds that keep the outer side, which
	 * the node probes with.
	 */
	JoinType	jointype;
	/* Per filter of an outer join in evaluation order: whether it runs in batches. */
	List	   *filter_batch;
	/* The residual clauses after the keys that run in batches, first. */
	/* Per residual clause in evaluation order: whether it runs in batches. */
	List	   *residual_batch;
	/* The planner's estimate of the inner rows, the table's first capacity. */
	int			inner_rows;
	/* The types of the scan tuple columns, for copying inner values. */
	int16	   *typlens;
	bool	   *typbyvals;

	/*
	 * The inner columns the parent asked for, in payload order: each
	 * record's payload is a word of their NULL bits, then a Datum each.
	 */
	int			npayload;
	int		   *payload_columns;

	/*
	 * The table: its index and chunks, with the chunks' bases and lengths
	 * in this process, room for chunk_slots of them, and the bytes they
	 * take together; the copies of by-reference inner values.
	 */
	MemoryContext table_context;
	MemoryContext values_context;
	TessTableRef table;
	void	  **chunk_bases;
	Size	   *chunk_lens;
	int			chunk_slots;
	Size		table_bytes;
	bool		built;
	/*
	 * The Bloom filter of the table's keys, or NULL; whether the first
	 * probes decided on it, and the valid rows and matches they counted.
	 */
	uint64	   *bloom;
	Size		bloom_words;
	bool		bloom_decided;
	/* The filter is a shared table's, and whether it was seen ready. */
	bool		bloom_shared;
	bool		bloom_ready;
	/* The outer child checks its rows against the filter: the join does not. */
	bool		bloom_below;
	uint64		sample_rows;
	uint64		sample_found;

	/* Buffers for one batch of either side, for capacity rows. */
	int			capacity;
	uint32	   *hashes;
	/* Insertion: each row's new record; probing: each row's match. */
	uint32	   *offsets;
	uint64	   *valid_bits;
	uint64	   *pending_bits;
	/* The payload of every row of an inner batch, one after another. */
	uint64	   *payload;
	/* The rows of the current round, and a copy the next step reads. */
	uint64	   *round_bits;
	uint64	   *next_bits;
	/* The published batch's mask, which the parent may narrow. */
	uint64	   *published_bits;
	/* Per row of the round: its record's NULL bits, then each kept column. */
	Datum	   *null_words;
	Datum	  **inner_values;
	bool	  **inner_isnull;
	/* What this round already gathered from the records. */
	bool		nulls_gathered;
	bool	   *gathered;

	/* The copies of the compact batch's by-reference outer values. */
	MemoryContext compact_context;
	/* The published batch is a compact one, not a round over the outer batch. */
	bool		output_compact;
	/* The records and rows the published batch reads: a round's or a compact batch's. */
	uint32	   *current_offsets;
	uint64	   *current_bits;

	/*
	 * Compact mode, for a table with duplicate keys: the pairs of the
	 * rounds are copied one after another into batches of
	 * JOIN_COMPACT_ROWS rows, the outer columns by value, the inner ones
	 * gathered from the pairs' records, instead of publishing every round
	 * over the outer batch's rows with a quarter of them selected.
	 */
	bool		compact;
	/* The outer scan columns the parent asked for, all passed by value. */
	int			nouter;
	int		   *outer_columns;
	/* Per scan column: its values in the compact batch; outer ones only. */
	Datum	  **compact_values;
	bool	  **compact_isnull;
	uint32		compact_offsets[JOIN_COMPACT_ROWS];
	uint64		compact_bits[1];
	/* The round being copied: its rows not yet copied, its outer columns. */
	bool		round_open;
	uint64	   *taken_bits;
	TessDatumColumn *round_columns;

	/* The outer batch whose rounds are being published, or NULL. */
	TessBatch  *outer_batch;
	/* The batch this node publishes: the outer rows of one round. */
	TessBatch	batch;
	/* Row mode: the column of every slot attribute, and the next row. */
	TessDatumColumn *columns;
	int			next_row;
	bool		serving;
	/* The residual join clauses over the pairs, or NULL; the scan tuple's layout. */
	TessQual   *qual;
	/* An outer join's filters over the rows it returns, or NULL. */
	TessQual   *filter;
	/*
	 * SEMI, ANTI and LEFT: the rows of the outer batch with a pair that
	 * passed the join clauses so far.
	 */
	uint64	   *matched_bits;
	/*
	 * LEFT: the published round extends the outer rows without a match
	 * with NULL inner columns, which these all-NULL columns give; a
	 * compact batch in progress holds it back for the next call.
	 */
	bool		null_round;
	bool		null_held;
	Datum	   *null_values;
	bool	   *null_isnull;
	/* The targets computed over the pairs, or NULL; the batch published then. */
	TessProjection *projection;
	List	   *computed;
	TessBatch  *published;
	TessLayout	scan_layout;
	/* Nothing is left to return. */
	bool		done;
	bool		compact_decided;

	/* Written by a kernel on failure only. */
	TessStatus	status;

	/*
	 * Spilling: NULL while the table fits in hash_mem. The rows of the
	 * outer batch still to be answered: those written to disk go later.
	 */
	JoinSpill  *spill;
	uint64	   *active_bits;

	/* The rows of the current table, and those whose key it held already. */
	uint64		build_rows;
	uint64		duplicates;
	/* Bit w: payload column w holds a NULL somewhere in the table. */
	uint64		null_columns;
	/* This participant's counters; the memory ones are set when read. */
	uint64		counters[JOIN_NCOUNTERS];
	/* The most memory the table and the copies took, in bytes. */
	Size		peak_memory;
	/* The counters of every participant, in a parallel plan. */
	TessSharedStats *stats;

	/*
	 * A shared build: the plan asks for one; the shared state in the DSM
	 * chunk, NULL when the plan runs without one (a Gather that launched
	 * no workers), and then the node builds a table of its own; this
	 * participant, whether it is attached to the build barrier, the chunk
	 * it appends to, the numbers of its chunks and their bytes, and the
	 * records it appended.
	 */
	bool		shared_mode;
	JoinShared *shared;
	/* The query's shared memory, kept for leaving after the Gather let go of it. */
	dsa_area   *area;
	TessBuildParticipant participant;
	bool		participating;
	void	   *own_base;
	Size		own_len;
	int		   *own_chunks;
	int			nown;
	int			own_slots;
	Size		own_bytes;
	uint64		appended;
	/*
	 * The chunks of by-reference values: their bases in this process by
	 * number, with room for value_slots; the chunk values are copied into
	 * and its length and bytes used; the chunks this process made and the
	 * bytes they take.
	 */
	char	  **value_bases;
	int			nvalue_chunks;
	int			value_slots;
	int			value_current;
	Size		value_len;
	Size		value_used;
	int			value_own;
	Size		value_bytes;
} TessHashJoinState;

static const CustomExecMethods join_exec_methods;

static void reset_values(TessHashJoinState *state);
static void start_spill(TessHashJoinState *state);
static void insert_spill(TessHashJoinState *state, TessBatch *batch);
static void finish_spill_build(TessHashJoinState *state);
static void spill_free(TessHashJoinState *state);
static Size spill_memory(JoinSpill *spill, uint64 *resident);
static void decide_compact(TessHashJoinState *state);

/* Raise the error a kernel stored, if the call failed. */
static inline void
check(TessHashJoinState *state, TessStatusCode code)
{
	if (code != TESS_OK)
		tess_status_report(&state->status);
}

static void child_column(TessBatch *batch, int column, const TessRowMask *rows,
						 TessColumnPurpose purpose, TessDatumColumn *result);

/*
 * The keys of a batch of one side: each key column, read for the selected
 * rows and hashed in key order, the first key's hash folding in the
 * others'; valid gets the rows whose keys are all non-NULL, since an
 * equality with NULL is never true. The table's keys point at the columns.
 */
static void
batch_keys(TessHashJoinState *state, TessBatch *batch, const int *columns,
		   const TessTableKeyKind *kinds, TessRowMask *valid)
{
	for (int key = 0; key < state->nkeys; key++)
	{
		TessDatumColumn *keys = &state->key_columns[key];
		bool		int8 = kinds[key] == TESS_TABLE_KEY_INT8;

		child_column(batch, columns[key], key == 0 ? &batch->rows : valid,
					 TESS_COLUMN_FOR_FILTER, keys);
		if (key == 0)
			check(state, (int8 ? state->kernels->int8_hash :
						  state->kernels->int4_hash) (keys, NULL, &batch->rows,
													  TESS_NULL_KEYS_REJECT,
													  state->hashes, valid,
													  &state->status));
		else
			check(state, (int8 ? state->kernels->int8_hash_next :
						  state->kernels->int4_hash_next) (keys, NULL,
														   TESS_NULL_KEYS_REJECT,
														   state->hashes, valid,
														   &state->status));
		state->table_keys[key].kind = kinds[key];
		state->table_keys[key].column = keys;
		state->table_keys[key].prepared = NULL;
	}
}

/* A column of a child's batch, checked. */
static void
child_column(TessBatch *batch, int column, const TessRowMask *rows,
			 TessColumnPurpose purpose, TessDatumColumn *result)
{
	*result = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
	batch->ops->get_datum_column(batch, column, rows, purpose, result);
	if (result->values == NULL || result->isnull == NULL ||
		result->nrows != batch->rows.nrows)
		elog(ERROR, "Tessera batch returned an invalid column");
}

/* Make the buffers hold batches of nrows rows. */
static void
reserve_rows(TessHashJoinState *state, int nrows)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	int			nwords;

	if (nrows <= state->capacity)
		return;
	nrows = Max(nrows, JOIN_INITIAL_ROWS);
	nwords = tess_row_mask_word_count(nrows);
	if (state->hashes != NULL)
	{
		pfree(state->hashes);
		pfree(state->offsets);
		pfree(state->valid_bits);
		pfree(state->pending_bits);
		pfree(state->payload);
		pfree(state->round_bits);
		pfree(state->next_bits);
		pfree(state->published_bits);
		pfree(state->taken_bits);
		pfree(state->null_words);
		pfree(state->matched_bits);
		pfree(state->active_bits);
		pfree(state->null_values);
		pfree(state->null_isnull);
		for (int word = 0; word < state->npayload; word++)
		{
			pfree(state->inner_values[word]);
			pfree(state->inner_isnull[word]);
		}
	}
	state->hashes = MemoryContextAllocZero(context, sizeof(uint32) * nrows);
	state->offsets = MemoryContextAllocZero(context, sizeof(uint32) * nrows);
	state->valid_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->pending_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->payload = MemoryContextAllocZero(context,
											mul_size(sizeof(uint64) * nrows,
													 1 + state->npayload));
	state->round_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->next_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->published_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->taken_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->null_words = MemoryContextAllocZero(context, sizeof(Datum) * nrows);
	state->matched_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->active_bits = MemoryContextAllocZero(context, sizeof(uint64) * nwords);
	state->null_values = MemoryContextAllocZero(context, sizeof(Datum) * nrows);
	state->null_isnull = MemoryContextAlloc(context, sizeof(bool) * nrows);
	memset(state->null_isnull, true, sizeof(bool) * nrows);
	/* Zeroed: rows outside a round are initialized memory, as batches promise. */
	for (int word = 0; word < state->npayload; word++)
	{
		state->inner_values[word] = MemoryContextAllocZero(context,
														   sizeof(Datum) * nrows);
		state->inner_isnull[word] = MemoryContextAllocZero(context,
														   sizeof(bool) * nrows);
	}
	state->capacity = nrows;
}

/* The bytes the table and the copies of inner values take now. */
static void
note_memory(TessHashJoinState *state)
{
	Size		memory = state->table_bytes + sizeof(uint64) * state->bloom_words +
		MemoryContextMemAllocated(state->values_context, true);

	if (state->spill != NULL)
		memory += spill_memory(state->spill, NULL);

	state->peak_memory = Max(state->peak_memory, memory);
}

/* Room for the bases and lengths of nchunks chunks in this process. */
static void
reserve_chunks(TessHashJoinState *state, int nchunks)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	int			slots = Max(state->chunk_slots, 16);

	if (nchunks <= state->chunk_slots)
		return;
	while (slots < nchunks)
		slots *= 2;
	if (state->chunk_bases == NULL)
	{
		state->chunk_bases = MemoryContextAlloc(context, sizeof(void *) * slots);
		state->chunk_lens = MemoryContextAlloc(context, sizeof(Size) * slots);
	}
	else
	{
		state->chunk_bases = repalloc(state->chunk_bases, sizeof(void *) * slots);
		state->chunk_lens = repalloc(state->chunk_lens, sizeof(Size) * slots);
	}
	state->chunk_slots = slots;
	state->table.chunks = state->chunk_bases;
	state->table.chunk_lens = state->chunk_lens;
}

/*
 * Take the filter back from the outer child before it goes: a new table
 * decides on its own.
 */
static void
take_back_bloom(TessHashJoinState *state)
{
	if (!state->bloom_below)
		return;
	(void) tess_input_set_key_filter(state->outer_input, NULL);
	state->bloom_below = false;
}

/* An empty table with no index yet, for a build that appends first. */
static void
create_table(TessHashJoinState *state)
{
	take_back_bloom(state);
	MemoryContextReset(state->table_context);
	MemoryContextReset(state->values_context);
	reset_values(state);
	/* A new table: decide on its filter again. */
	state->bloom = NULL;
	state->bloom_words = 0;
	state->bloom_decided = false;
	state->sample_rows = 0;
	state->sample_found = 0;
	state->table.index = NULL;
	state->table.index_len = 0;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	reserve_chunks(state, 1);
	note_memory(state);
}

/*
 * The chunks of a serial table and of its values past the first: up to
 * the largest, and at most an eighth of hash_mem, so that a table spills
 * only near its limit.
 */
static Size
serial_chunk_len(Size largest)
{
	return Max(JOIN_FIRST_CHUNK,
			   Min(largest, TYPEALIGN_DOWN(8, get_hash_memory_limit() / 8)));
}

/* Another chunk for the serial table, the last one being full. */
static void
add_table_chunk(TessHashJoinState *state)
{
	int			chunk = state->table.nchunks;
	Size		len = chunk == 0 ? JOIN_FIRST_CHUNK : serial_chunk_len(JOIN_CHUNK_LEN);
	void	   *base;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	reserve_chunks(state, chunk + 1);
	base = MemoryContextAlloc(state->table_context, len);
	check(state, state->kernels->table_chunk_init(base, len, &state->status));
	state->chunk_bases[chunk] = base;
	state->chunk_lens[chunk] = len;
	state->table.nchunks++;
	state->table_bytes += len;
	state->counters[JOIN_CHUNKS]++;
}

/*
 * The index for the records appended, made once the inner side is read,
 * and the records linked into it, those of a key next to each other: the
 * rounds step from one to the next, and a table without duplicates has
 * no second round.
 */
static void
index_table(TessHashJoinState *state)
{
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	uint64		capacity = Max(state->build_rows, JOIN_INITIAL_ROWS);
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	Size		size;

	/* One index per build, made once every chunk is appended. */
	Assert(state->table.index == NULL);
	check(state, state->kernels->table_size(state->nkeys, state->inner_kinds,
											payload_size, capacity,
											&size, &state->status));
	state->table.index = MemoryContextAllocExtended(state->table_context, size,
													MCXT_ALLOC_HUGE);
	state->table.index_len = size;
	check(state, state->kernels->table_create(state->table.index, size,
											  state->nkeys, state->inner_kinds,
											  payload_size, capacity,
											  &state->status));
	state->table_bytes += size;
	for (int chunk = 0; chunk < state->table.nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;
		uint64		duplicates;

		check(state, state->kernels->table_link_grouped(&state->table, chunk,
														&from, NULL,
														&duplicates,
														&state->status));
		state->duplicates += duplicates;
	}
	check(state, state->kernels->table_stats(&state->table, &stats,
											 &state->status));
	state->counters[JOIN_BUCKETS] += stats.buckets;
	note_memory(state);
}

static dsa_area *query_dsa(TessHashJoinState *state);

/* Room for the bases of nchunks value chunks in this process. */
static void
reserve_values(TessHashJoinState *state, int nchunks)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	int			slots = Max(state->value_slots, 16);

	if (nchunks <= state->value_slots)
		return;
	while (slots < nchunks)
		slots *= 2;
	state->value_bases = state->value_bases == NULL ?
		MemoryContextAllocZero(context, sizeof(char *) * slots) :
		repalloc0(state->value_bases, sizeof(char *) * state->value_slots,
				  sizeof(char *) * slots);
	state->value_slots = slots;
}

/* Forget the value chunks, which went with the table. */
static void
reset_values(TessHashJoinState *state)
{
	state->nvalue_chunks = 0;
	state->value_current = -1;
	state->value_len = 0;
	state->value_used = 0;
	state->value_own = 0;
	state->value_bytes = 0;
}

/*
 * A value chunk of len bytes: in the node's memory, or in the query's
 * shared memory, numbered under the lock and entered in the table's list.
 * Returns its number.
 */
static int
new_value_chunk(TessHashJoinState *state, Size len)
{
	int			number;
	char	   *base;

	if (state->shared != NULL)
	{
		dsa_area   *area = query_dsa(state);
		dsa_pointer block = dsa_allocate_extended(area, add_size(JOIN_CHUNK_HEADER, len),
												  DSA_ALLOC_HUGE);
		JoinChunk  *header = dsa_get_address(area, block);

		SpinLockAcquire(&state->shared->lock);
		number = (int) state->shared->next_value_chunk++;
		header->next = state->shared->values;
		state->shared->values = block;
		SpinLockRelease(&state->shared->lock);
		header->number = number;
		header->len = len;
		base = (char *) header + JOIN_CHUNK_HEADER;
		state->value_bytes = add_size(state->value_bytes, JOIN_CHUNK_HEADER);
	}
	else
	{
		number = state->nvalue_chunks;
		base = MemoryContextAllocExtended(state->values_context, len, MCXT_ALLOC_HUGE);
	}
	if (number >= INT_MAX - 1)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin cannot hold more chunks of values")));
	reserve_values(state, number + 1);
	state->value_bases[number] = base;
	state->nvalue_chunks = Max(state->nvalue_chunks, number + 1);
	state->value_own++;
	state->value_bytes = add_size(state->value_bytes, len);
	return number;
}

/*
 * Copy a by-reference inner value into the table's value chunks and
 * return its reference: the bytes datumCopy would copy, an expanded
 * object flattened.
 */
static uint64
store_value(TessHashJoinState *state, Datum value, int16 typlen)
{
	ExpandedObjectHeader *expanded = NULL;
	Size		size;
	Size		aligned;
	int			number;
	Size		byte;

	if (typlen == -1 && VARATT_IS_EXTERNAL_EXPANDED(DatumGetPointer(value)))
	{
		expanded = DatumGetEOHP(value);
		size = EOH_get_flat_size(expanded);
	}
	else
		size = datumGetSize(value, false, typlen);
	aligned = MAXALIGN(size);
	if (aligned > JOIN_VALUE_CHUNK / 4)
	{
		number = new_value_chunk(state, aligned);
		byte = 0;
	}
	else
	{
		if (state->value_current < 0 ||
			state->value_used + aligned > state->value_len)
		{
			Size		len = state->value_own == 0 ? JOIN_VALUE_FIRST :
				state->shared != NULL ? JOIN_VALUE_CHUNK :
				serial_chunk_len(JOIN_VALUE_CHUNK);

			state->value_len = Max(len, aligned);
			state->value_current = new_value_chunk(state, state->value_len);
			state->value_used = 0;
		}
		number = state->value_current;
		byte = state->value_used;
		state->value_used += aligned;
	}
	if (expanded != NULL)
		EOH_flatten_into(expanded, state->value_bases[number] + byte, size);
	else
		memcpy(state->value_bases[number] + byte, DatumGetPointer(value), size);
	return JOIN_VALUE_REF(number, byte);
}

/*
 * The payload of every valid row: the NULL bits of the kept inner
 * columns, then each value, a by-reference one copied into the node's
 * memory, where it lives as long as the table.
 */
static void
fill_payload(TessHashJoinState *state, TessBatch *batch, const TessRowMask *valid)
{
	int			width = 1 + state->npayload;
	int			row = -1;

	while ((row = tess_row_mask_next(valid, row)) >= 0)
		state->payload[row * width] = 0;
	/* The columns' NULL bits, gathered below for the whole table. */
	for (int word = 0; word < state->npayload; word++)
	{
		int			scan_column = state->payload_columns[word];
		int16		typlen = state->typlens[scan_column];
		bool		byval = state->typbyvals[scan_column];
		TessDatumColumn values;

		child_column(batch, state->child_columns[scan_column], valid,
					 TESS_COLUMN_FOR_PROJECTION, &values);
		row = -1;
		while ((row = tess_row_mask_next(valid, row)) >= 0)
		{
			uint64	   *record = &state->payload[row * width];

			if (values.isnull[row])
			{
				record[0] |= UINT64CONST(1) << word;
				record[1 + word] = 0;
				state->null_columns |= UINT64CONST(1) << word;
			}
			else if (byval)
				record[1 + word] = values.values[row];
			else
				record[1 + word] = store_value(state, values.values[row], typlen);
		}
	}
}

/*
 * Append the pending rows of an inner batch to chunk `chunk` of table:
 * false when it had room for none of them.
 */
static bool
append_rows(TessHashJoinState *state, const TessTableRef *table, int chunk,
			TessRowMask *pending)
{
	int			before = tess_row_mask_count(pending);

	check(state, state->kernels->table_append(table, chunk,
											  sizeof(uint64) * (1 + state->npayload),
											  state->hashes, state->nkeys,
											  state->table_keys,
											  (const uint8 *) state->payload,
											  pending, state->offsets,
											  &state->status));
	return tess_row_mask_count(pending) < before;
}

/*
 * The valid rows of an inner batch, with their payload filled, into
 * pending: their count.
 */
static int
prepare_inner(TessHashJoinState *state, TessBatch *batch, TessRowMask *pending)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	int			count;

	reserve_rows(state, nrows);
	/* A shorter batch than the last: no bits past its rows may remain. */
	memset(state->valid_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->valid_bits};
	*pending = (TessRowMask) {nrows, state->pending_bits};
	batch_keys(state, batch, state->inner_keys, state->inner_kinds, &valid);
	count = tess_row_mask_count(&valid);
	if (count == 0)
		return 0;
	fill_payload(state, batch, &valid);
	memcpy(state->pending_bits, state->valid_bits, sizeof(uint64) * nwords);
	return count;
}

/*
 * Append the rows of one inner batch, adding chunks until they fit; once
 * the table outgrows hash_mem, it spills, and the rows go into its
 * partitions.
 */
static void
insert_batch(TessHashJoinState *state, TessBatch *batch)
{
	TessRowMask pending;
	int			count;
	bool		fresh = false;

	/* A batch adds a chunk of records and one of values at most. */
	if (state->spill == NULL &&
		state->table_bytes + state->value_bytes +
		2 * serial_chunk_len(JOIN_CHUNK_LEN) > get_hash_memory_limit())
		start_spill(state);
	if (state->spill != NULL)
	{
		insert_spill(state, batch);
		return;
	}
	count = prepare_inner(state, batch, &pending);
	if (count == 0)
		return;
	if (state->table.nchunks == 0)
	{
		add_table_chunk(state);
		fresh = true;
	}
	for (;;)
	{
		bool		appended = append_rows(state, &state->table,
										   state->table.nchunks - 1, &pending);

		if (tess_row_mask_count(&pending) == 0)
			break;
		if (!appended && fresh)
			elog(ERROR, "TessHashJoin cannot fit a row of its table in a chunk");
		add_table_chunk(state);
		fresh = true;
	}
	state->build_rows += count;
	state->counters[JOIN_BUILD_ROWS] += count;
	note_memory(state);
}

/* Read every batch of the inner child into a new table. */
static void
build_table(TessHashJoinState *state)
{
	spill_free(state);
	create_table(state);
	/* Another table, maybe with other duplicates: decide compact mode again. */
	state->compact_decided = false;
	state->build_rows = 0;
	state->duplicates = 0;
	state->null_columns = 0;
	/* A column without NULLs is never gathered for them: no flag may stay set. */
	if (state->inner_isnull != NULL && state->capacity > 0)
		for (int word = 0; word < state->npayload; word++)
			memset(state->inner_isnull[word], 0, sizeof(bool) * state->capacity);
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->inner_input);

		if (batch == NULL)
			break;
		if (tess_row_mask_count(&batch->rows) > 0)
			insert_batch(state, batch);
		tess_input_finish(state->inner_input);
	}
	if (state->spill != NULL)
		finish_spill_build(state);
	else
		index_table(state);
	state->counters[JOIN_BUILDS]++;
	state->built = true;
}

static void spill_get_column(TessBatch *batch, int column, const TessRowMask *rows,
							 TessColumnPurpose purpose, TessDatumColumn *result);

static const TessBatchOps spill_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = spill_get_column,
};

/*
 * Spilling (docs/spill.md). The table fits until the bytes it takes pass
 * hash_mem; then its records go into partitions and the build goes on
 * partitioned. Each side keeps its partitions' records in chunks of the
 * table's format and their by-reference values in value chunks; a chunk
 * written to disk goes after the value chunks opened since the last one,
 * so that a partition's file, read in order, gives a chunk's values
 * before its records.
 */

static void
grow_ints(MemoryContext context, int **array, int *slots, int needed)
{
	int			grown = Max(*slots, 8);

	if (needed <= *slots)
		return;
	while (grown < needed)
		grown *= 2;
	*array = *array == NULL ?
		MemoryContextAlloc(context, sizeof(int) * grown) :
		repalloc(*array, sizeof(int) * grown);
	*slots = grown;
}

static inline uint32
spill_partition(const JoinSpill *spill, uint32 hash)
{
	return (hash >> spill->shift) & (uint32) (spill->npartitions - 1);
}

static inline uint64
chunk_used(const void *base)
{
	return *(const uint64 *) base;
}

static void
side_sync(SpillSide *side)
{
	side->ref.chunks = side->bases;
	side->ref.chunk_lens = side->lens;
	side->ref.nchunks = side->nchunks;
}

/*
 * A side of nkeys keys of the kinds and a payload of a word of NULL bits
 * and nwords words of the types, with no chunk yet but the empty one and
 * an index for the layout only.
 */
static void
side_init(TessHashJoinState *state, SpillSide *side, int nkeys, const TessTableKeyKind *kinds, int nwords,
		  const int16 *typlens, const bool *byvals, Size chunk_len,
		  bool resident)
{
	JoinSpill  *spill = state->spill;
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	void	   *empty;

	if (resident)
		side->context = AllocSetContextCreate(spill->context,
											  "TessHashJoin inner partitions",
											  ALLOCSET_DEFAULT_SIZES);
	else
		side->context = AllocSetContextCreate(spill->context,
											  "TessHashJoin outer partitions",
											  ALLOCSET_DEFAULT_SIZES);
	side->nkeys = nkeys;
	memcpy(side->kinds, kinds, sizeof(TessTableKeyKind) * nkeys);
	side->nwords = nwords;
	side->typlens = MemoryContextAlloc(side->context, sizeof(int16) * Max(nwords, 1));
	side->byvals = MemoryContextAlloc(side->context, sizeof(bool) * Max(nwords, 1));
	if (nwords > 0)
	{
		memcpy(side->typlens, typlens, sizeof(int16) * nwords);
		memcpy(side->byvals, byvals, sizeof(bool) * nwords);
	}
	side->payload_size = sizeof(uint64) * (1 + nwords);
	side->chunk_len = chunk_len;
	check(state, state->kernels->table_size(nkeys, kinds, side->payload_size,
											JOIN_INITIAL_ROWS,
											&side->ref.index_len, &state->status));
	side->ref.index = MemoryContextAllocZero(side->context, side->ref.index_len);
	check(state, state->kernels->table_create(side->ref.index, side->ref.index_len,
											  nkeys, kinds, side->payload_size,
											  JOIN_INITIAL_ROWS, &state->status));
	side->slots = 16;
	side->bases = MemoryContextAlloc(side->context, sizeof(void *) * side->slots);
	side->lens = MemoryContextAlloc(side->context, sizeof(Size) * side->slots);
	empty = MemoryContextAlloc(side->context, TESS_TABLE_CHUNK_HEADER);
	check(state, state->kernels->table_chunk_init(empty, TESS_TABLE_CHUNK_HEADER,
												  &state->status));
	side->bases[0] = empty;
	side->lens[0] = TESS_TABLE_CHUNK_HEADER;
	side->nchunks = 1;
	side_sync(side);
	side->npartitions = spill->npartitions;
	side->current = MemoryContextAllocZero(side->context,
										   sizeof(uint32) * spill->npartitions);
	side->parts = MemoryContextAllocZero(side->context,
										 sizeof(SpillPart) * spill->npartitions);
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		side->parts[partition].value_current = -1;
		side->parts[partition].resident = resident;
	}
	side->queue = MemoryContextAlloc(side->context, sizeof(int) * spill->npartitions);
	check(state, state->kernels->table_fingerprint(&side->ref, &side->fingerprint,
												   &state->status));
	config.parent_context = side->context;
	config.kernels = state->kernels;
	config.npartitions = spill->npartitions;
	config.level = 0;
	config.fingerprint = side->fingerprint;
	config.max_len = (uint64) MaxAllocHugeSize;
	side->file = tess_spill_create(&config);
}

/* A new chunk that the partition appends to from now on; its index. */
static int
side_add_chunk(TessHashJoinState *state, SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];
	int			index = side->nchunks;
	void	   *base;

	if (index == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	if (index == side->slots)
	{
		side->slots *= 2;
		side->bases = repalloc(side->bases, sizeof(void *) * side->slots);
		side->lens = repalloc(side->lens, sizeof(Size) * side->slots);
	}
	base = MemoryContextAlloc(side->context, side->chunk_len);
	check(state, state->kernels->table_chunk_init(base, side->chunk_len,
												  &state->status));
	side->bases[index] = base;
	side->lens[index] = side->chunk_len;
	side->nchunks++;
	side_sync(side);
	grow_ints(side->context, &part->chunks, &part->chunk_slots, part->nchunks + 1);
	part->chunks[part->nchunks++] = index;
	side->current[partition] = index;
	part->bytes += side->chunk_len;
	side->bytes += side->chunk_len;
	if (side == &state->spill->build)
		state->counters[JOIN_CHUNKS]++;
	return index;
}

/* Drop the chunks whose base is gone and number the others anew. */
static void
side_compact(SpillSide *side)
{
	int		   *map = palloc(sizeof(int) * side->nchunks);
	int			kept = 1;

	map[0] = 0;
	for (int index = 1; index < side->nchunks; index++)
	{
		if (side->bases[index] == NULL)
		{
			map[index] = 0;
			continue;
		}
		map[index] = kept;
		side->bases[kept] = side->bases[index];
		side->lens[kept] = side->lens[index];
		kept++;
	}
	for (int partition = 0; partition < side->npartitions; partition++)
	{
		SpillPart  *part = &side->parts[partition];
		int			count = 0;

		side->current[partition] = map[side->current[partition]];
		for (int chunk = 0; chunk < part->nchunks; chunk++)
			if (map[part->chunks[chunk]] != 0)
				part->chunks[count++] = map[part->chunks[chunk]];
		part->nchunks = count;
	}
	side->nchunks = kept;
	side_sync(side);
	pfree(map);
}

static void
write_block(TessHashJoinState *state, SpillSide *side, int partition,
			TessSpillKind kind, uint32 number, const void *body, Size len)
{
	tess_spill_write(side->file, partition, kind, number, body, len, NULL);
	side->parts[partition].written = true;
	side->parts[partition].disk_bytes += len;
	state->counters[JOIN_SPILLED]++;
	state->counters[JOIN_DISK] += TESS_SPILL_HEADER_SIZE + len;
}

/* Write the partition's value chunks in memory and free them. */
static void
side_write_values(TessHashJoinState *state, SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];

	for (int index = 0; index < part->nvalues; index++)
	{
		int			number = part->values[index];

		write_block(state, side, partition, TESS_SPILL_VALUES, number,
					side->value_bases[number], side->value_lens[number]);
		pfree(side->value_bases[number]);
		side->value_bases[number] = NULL;
		side->bytes -= side->value_allocated[number];
		part->bytes -= side->value_allocated[number];
	}
	part->nvalues = 0;
	part->value_current = -1;
	part->value_bytes = 0;
}

/* Write chunk `index` of the partition, unless it holds no record. */
static void
side_write_records(TessHashJoinState *state, SpillSide *side, int partition,
				   int index)
{
	Size		used = chunk_used(side->bases[index]);

	if (used > TESS_TABLE_CHUNK_HEADER)
		write_block(state, side, partition, TESS_SPILL_RECORDS,
					side->next_number++, side->bases[index], used);
}

/*
 * A partition on disk writes its values and its tail, which then takes
 * the next records.
 */
static void
side_flush(TessHashJoinState *state, SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];
	int			index = side->current[partition];

	Assert(!part->resident);
	side_write_values(state, side, partition);
	part->queued = false;
	if (index == 0)
		return;
	side_write_records(state, side, partition, index);
	check(state, state->kernels->table_chunk_init(side->bases[index],
												  side->lens[index],
												  &state->status));
}

/*
 * A resident partition goes to disk: its values, then every chunk; the
 * last chunk stays as its tail, emptied.
 */
static void
side_demote(TessHashJoinState *state, SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];
	int			tail = side->current[partition];

	part->resident = false;
	side_write_values(state, side, partition);
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		int			index = part->chunks[chunk];

		side_write_records(state, side, partition, index);
		if (index == tail)
			continue;
		pfree(side->bases[index]);
		side->bases[index] = NULL;
		side->bytes -= side->lens[index];
		part->bytes -= side->lens[index];
	}
	if (tail != 0)
		check(state, state->kernels->table_chunk_init(side->bases[tail],
													  side->lens[tail],
													  &state->status));
	side_compact(side);
}

/* A value chunk of len bytes for the partition; its number. */
static int
side_value_chunk(SpillSide *side, int partition, Size len)
{
	SpillPart  *part = &side->parts[partition];
	int			number = side->nvalues;

	if (number >= INT_MAX - 1)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin cannot hold more chunks of values")));
	if (number == side->value_slots)
	{
		int			slots = Max(side->value_slots * 2, 16);

		side->value_bases = side->value_bases == NULL ?
			MemoryContextAllocZero(side->context, sizeof(char *) * slots) :
			repalloc0(side->value_bases, sizeof(char *) * side->value_slots,
					  sizeof(char *) * slots);
		side->value_lens = side->value_lens == NULL ?
			MemoryContextAlloc(side->context, sizeof(Size) * slots) :
			repalloc(side->value_lens, sizeof(Size) * slots);
		side->value_allocated = side->value_allocated == NULL ?
			MemoryContextAlloc(side->context, sizeof(Size) * slots) :
			repalloc(side->value_allocated, sizeof(Size) * slots);
		side->value_slots = slots;
	}
	side->value_bases[number] = MemoryContextAllocExtended(side->context, len,
														   MCXT_ALLOC_HUGE);
	side->value_lens[number] = 0;
	side->value_allocated[number] = len;
	side->nvalues++;
	grow_ints(side->context, &part->values, &part->value_slots, part->nvalues + 1);
	part->values[part->nvalues++] = number;
	part->value_bytes += len;
	part->bytes += len;
	side->bytes += len;
	return number;
}

/*
 * Copy a by-reference value into the partition's value chunks, as
 * store_value does into the table's, and return its reference. A
 * partition on disk whose values outgrow a chunk is queued to be written.
 */
static uint64
side_store(SpillSide *side, int partition, Datum value, int16 typlen)
{
	SpillPart  *part = &side->parts[partition];
	ExpandedObjectHeader *expanded = NULL;
	Size		size;
	Size		aligned;
	int			number;
	Size		byte;

	if (typlen == -1 && VARATT_IS_EXTERNAL_EXPANDED(DatumGetPointer(value)))
	{
		expanded = DatumGetEOHP(value);
		size = EOH_get_flat_size(expanded);
	}
	else
		size = datumGetSize(value, false, typlen);
	aligned = MAXALIGN(size);
	if (aligned > side->chunk_len / 4)
	{
		number = side_value_chunk(side, partition, aligned);
		byte = 0;
		side->value_lens[number] = aligned;
	}
	else
	{
		if (part->value_current < 0 ||
			part->value_used + aligned > part->value_len)
		{
			part->value_current = side_value_chunk(side, partition, side->chunk_len);
			part->value_len = side->chunk_len;
			part->value_used = 0;
		}
		number = part->value_current;
		byte = part->value_used;
		part->value_used += aligned;
		side->value_lens[number] = part->value_used;
	}
	if (expanded != NULL)
		EOH_flatten_into(expanded, side->value_bases[number] + byte, size);
	else
		memcpy(side->value_bases[number] + byte, DatumGetPointer(value), size);
	if (!part->resident && !part->queued && part->value_bytes > side->chunk_len)
	{
		part->queued = true;
		side->queue[side->nqueue++] = partition;
	}
	return JOIN_VALUE_REF(number, byte);
}

/* Write the partitions whose values outgrew a chunk. */
static void
side_flush_queue(TessHashJoinState *state, SpillSide *side)
{
	for (int index = 0; index < side->nqueue; index++)
		if (side->parts[side->queue[index]].queued)
			side_flush(state, side, side->queue[index]);
	side->nqueue = 0;
}

/*
 * The bytes spilling takes in memory, with 8 bytes of index per resident
 * row; the resident rows into *resident unless NULL.
 */
static Size
spill_memory(JoinSpill *spill, uint64 *resident)
{
	uint64		rows = 0;

	for (int partition = 0; partition < spill->npartitions; partition++)
		if (spill->build.parts[partition].resident)
			rows += spill->build.parts[partition].rows;
	if (resident != NULL)
		*resident = rows;
	return spill->build.bytes + spill->probe.bytes +
		sizeof(uint64) * spill->bloom_words + rows * sizeof(uint64) +
		MemoryContextMemAllocated(spill->part_context, true) +
		MemoryContextMemAllocated(spill->block_context, true);
}

/*
 * Keep spilling within hash_mem while building: while it takes more, the
 * largest resident partition goes to disk. Room stays for the outer
 * side's tails, a chunk of records and one of values per partition,
 * which come once the probing starts. The tails stay: the chunk size
 * bounds them, and writing them sooner would write chunks of a few rows.
 */
static void
make_room(TessHashJoinState *state, bool building)
{
	JoinSpill  *spill = state->spill;
	Size		limit = get_hash_memory_limit();
	Size		outer = (Size) spill->npartitions * 2 * spill->probe.chunk_len;

	while (building && spill_memory(spill, NULL) + outer > limit)
	{
		int			largest = -1;
		Size		bytes = 0;

		for (int partition = 0; partition < spill->npartitions; partition++)
		{
			SpillPart  *part = &spill->build.parts[partition];

			if (part->resident && part->bytes > bytes)
			{
				largest = partition;
				bytes = part->bytes;
			}
		}
		if (largest < 0)
			break;
		side_demote(state, &spill->build, largest);
	}
	note_memory(state);
}

/*
 * The table outgrew hash_mem: choose the partitions, the power of two
 * that makes the expected inner side about half of hash_mem per
 * partition, and chunks that let every partition's tail fit; set both
 * sides up, and move the table built so far into the partitions.
 */
static void split_table(TessHashJoinState *state);

static void
start_spill(TessHashJoinState *state)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	Size		limit = get_hash_memory_limit();
	double		bytes = (double) state->table_bytes + state->value_bytes;
	double		expected = bytes * 2;
	Size		record = 16 + 8 * state->nkeys + sizeof(uint64) * (1 + state->npayload);
	Size		chunk_len;
	int			npartitions = JOIN_SPILL_MIN_PARTITIONS;
	JoinSpill  *spill;
	int16	   *typlens;
	bool	   *byvals;
	int			nchild = 0;
	int			nstored = 0;
	uint64		expected_rows;

	if (state->build_rows > 0)
		expected = Max(expected, bytes / state->build_rows * state->inner_rows);
	/*
	 * Each partition keeps a tail of records and one of values on each
	 * side: at the smallest chunk, half of hash_mem bounds them all.
	 */
	while (npartitions < JOIN_SPILL_MAX_PARTITIONS &&
		   (double) npartitions * (limit / 2) < expected &&
		   (Size) npartitions * 2 * 4 * JOIN_SPILL_MIN_CHUNK <= limit / 2)
		npartitions *= 2;
	chunk_len = limit / (16 * npartitions);
	chunk_len = Min(chunk_len, JOIN_CHUNK_LEN);
	chunk_len = Max(chunk_len, JOIN_SPILL_MIN_CHUNK);
	chunk_len = Max(chunk_len, TESS_TABLE_CHUNK_HEADER + 4 * record);
	chunk_len = TYPEALIGN_DOWN(8, chunk_len);

	spill = MemoryContextAllocZero(context, sizeof(JoinSpill));
	state->spill = spill;
	spill->context = AllocSetContextCreate(context, "TessHashJoin spill",
										   ALLOCSET_DEFAULT_SIZES);
	spill->part_context = AllocSetContextCreate(spill->context,
												"TessHashJoin partition",
												ALLOCSET_DEFAULT_SIZES);
	spill->block_context = AllocSetContextCreate(spill->context,
												 "TessHashJoin outer block",
												 ALLOCSET_DEFAULT_SIZES);
	spill->npartitions = npartitions;
	spill->shift = 0;
	spill->partition = -1;
	state->counters[JOIN_BATCHES] = Max(state->counters[JOIN_BATCHES],
										(uint64) npartitions);

	/* The inner side: the table's own payload. */
	typlens = palloc(sizeof(int16) * Max(state->npayload, 1));
	byvals = palloc(sizeof(bool) * Max(state->npayload, 1));
	for (int word = 0; word < state->npayload; word++)
	{
		typlens[word] = state->typlens[state->payload_columns[word]];
		byvals[word] = state->typbyvals[state->payload_columns[word]];
	}
	side_init(state, &spill->build, state->nkeys, state->inner_kinds, state->npayload, typlens,
			  byvals, chunk_len, true);

	/*
	 * The outer side: the columns of the outer child the node reads, the
	 * keys and the columns asked for, each once.
	 */
	for (int key = 0; key < state->nkeys; key++)
		nchild = Max(nchild, state->outer_keys[key] + 1);
	for (int index = 0; index < state->nouter; index++)
		nchild = Max(nchild, state->child_columns[state->outer_columns[index]] + 1);
	spill->nchild = nchild;
	spill->word_of = MemoryContextAllocZero(spill->context, sizeof(int) * Max(nchild, 1));
	spill->stored = MemoryContextAlloc(spill->context,
									   sizeof(int) * (state->nkeys + state->nouter));
	typlens = repalloc(typlens, sizeof(int16) * (state->nkeys + state->nouter));
	byvals = repalloc(byvals, sizeof(bool) * (state->nkeys + state->nouter));
	for (int index = 0; index < state->nouter; index++)
	{
		int			scan = state->outer_columns[index];
		int			child = state->child_columns[scan];

		if (spill->word_of[child] != 0)
			continue;
		spill->stored[nstored] = child;
		typlens[nstored] = state->typlens[scan];
		byvals[nstored] = state->typbyvals[scan];
		spill->word_of[child] = ++nstored;
	}
	for (int key = 0; key < state->nkeys; key++)
	{
		int			child = state->outer_keys[key];

		if (spill->word_of[child] != 0)
			continue;
		spill->stored[nstored] = child;
		typlens[nstored] = sizeof(Datum);
		byvals[nstored] = true;
		spill->word_of[child] = ++nstored;
	}
	if (nstored > 64)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin cannot spill more than 64 outer columns")));
	side_init(state, &spill->probe, state->nkeys, state->outer_kinds, nstored, typlens, byvals,
			  chunk_len, false);
	pfree(typlens);
	pfree(byvals);
	spill->build_children = MemoryContextAlloc(spill->context,
											   sizeof(int) * Max(state->npayload, 1));
	for (int word = 0; word < state->npayload; word++)
		spill->build_children[word] = state->child_columns[state->payload_columns[word]];
	spill->columns = MemoryContextAlloc(spill->context,
										sizeof(TessDatumColumn) *
										Max(Max(state->npayload, nstored), 1));
	spill->values = MemoryContextAlloc(spill->context, sizeof(Datum *) * Max(nstored, 1));
	spill->isnull = MemoryContextAlloc(spill->context, sizeof(bool *) * Max(nstored, 1));
	for (int word = 0; word < nstored; word++)
	{
		spill->values[word] = MemoryContextAllocZero(spill->context,
													 sizeof(Datum) * JOIN_COMPACT_ROWS);
		spill->isnull[word] = MemoryContextAllocZero(spill->context,
													 sizeof(bool) * JOIN_COMPACT_ROWS);
	}
	spill->batch = (TessBatch) {
		TESS_ABI_INITIALIZER(TESS_BATCH_ABI_VERSION, TessBatch),
	};
	spill->batch.table_oid = InvalidOid;
	spill->batch.ops = &spill_batch_ops;
	spill->batch.private_data = state;
	spill->batch.rows.bits = spill->bits;

	/* A filter of every inner row, sized for the rows expected. */
	state->counters[JOIN_BLOOM_FILTERS]++;
	expected_rows = Max((uint64) state->inner_rows, state->build_rows * 2);
	check(state, state->kernels->table_bloom_words(Max(expected_rows, 1),
												   &spill->bloom_words,
												   &state->status));
	spill->bloom = MemoryContextAllocExtended(spill->context,
											  mul_size(sizeof(uint64), spill->bloom_words),
											  MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
	split_table(state);
}

/* Room for the payload of a batch of the node's capacity, of either side. */
static void
spill_reserve(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	if (spill->payload_rows >= state->capacity)
		return;
	if (spill->payload != NULL)
	{
		pfree(spill->payload);
		pfree(spill->before_bits);
	}
	spill->payload = MemoryContextAlloc(spill->context,
										mul_size(sizeof(uint64) * state->capacity,
												 1 + Max(spill->build.nwords,
														 spill->probe.nwords)));
	spill->before_bits = MemoryContextAlloc(spill->context,
											sizeof(uint64) *
											tess_row_mask_word_count(state->capacity));
	spill->payload_rows = state->capacity;
}

/*
 * The table built before the first spill, into the partitions: its
 * chunks one by one, each split by the kernel into the partitions'
 * chunks, the by-reference values of its records copied into the value
 * chunks of their partitions, and then freed.
 */
static void
split_table(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	uint32		offsets[JOIN_COMPACT_ROWS];
	uint32		hashes[JOIN_COMPACT_ROWS];
	bool		byref = false;

	for (int word = 0; word < side->nwords; word++)
		byref |= !side->byvals[word];
	for (int chunk = 0; chunk < state->table.nchunks; chunk++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;
		int			source;

		/* The chunk joins the side's for the call, as a chunk of no partition. */
		if (side->nchunks == side->slots)
		{
			side->slots *= 2;
			side->bases = repalloc(side->bases, sizeof(void *) * side->slots);
			side->lens = repalloc(side->lens, sizeof(Size) * side->slots);
		}
		source = side->nchunks++;
		side->bases[source] = state->chunk_bases[chunk];
		side->lens[source] = state->chunk_lens[chunk];
		side_sync(side);
		for (;;)
		{
			int			count;
			int			full;
			uint64		bits;

			check(state, state->kernels->table_split(&side->ref, side->nkeys,
													 side->kinds, side->payload_size,
													 side->current, spill->npartitions,
													 spill->shift, source, &from,
													 JOIN_COMPACT_ROWS, offsets, hashes,
													 &count, &full, &state->status));
			bits = count == 64 ? ~UINT64CONST(0) : (UINT64CONST(1) << count) - 1;
			if (count > 0)
				check(state, state->kernels->bloom_add(spill->bloom, spill->bloom_words,
													   hashes,
													   &(TessRowMask) {count, &bits},
													   &state->status));
			for (int index = 0; index < count; index++)
			{
				int			partition = spill_partition(spill, hashes[index]);
				uint8	   *payload;

				side->parts[partition].rows++;
				if (!byref)
					continue;
				check(state, state->kernels->table_payload(&side->ref, offsets[index],
														   &payload, &state->status));
				for (int word = 0; word < side->nwords; word++)
				{
					uint64	   *slot = (uint64 *) payload + 1 + word;
					char	   *value;

					if (side->byvals[word] || *slot == 0)
						continue;
					value = state->value_bases[(*slot >> 32) - 1] + (*slot & 0xFFFFFFFF);
					*slot = side_store(side, partition, PointerGetDatum(value),
									   side->typlens[word]);
				}
			}
			if (full >= 0)
				side_add_chunk(state, side, full);
			else if (count == 0)
				break;
		}
		/* The chunk leaves the side, and its memory goes. */
		pfree(side->bases[source]);
		side->bases[source] = NULL;
		side_compact(side);
	}
	spill->total_rows = state->build_rows;
	MemoryContextReset(state->table_context);
	/* The by-reference values moved into the partitions' value chunks. */
	MemoryContextReset(state->values_context);
	reset_values(state);
	if (state->value_bases != NULL)
		pfree(state->value_bases);
	state->value_bases = NULL;
	state->value_slots = 0;
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	make_room(state, true);
}

/*
 * Make room for the rows still pending: a partition whose chunk is full,
 * or that has none, gets a new one while resident or without a tail, and
 * writes its tail otherwise.
 */
static void
make_chunks(TessHashJoinState *state, SpillSide *side, const TessRowMask *pending,
			const uint32 *hashes)
{
	JoinSpill  *spill = state->spill;
	int			row = -1;

	spill->stamp++;
	while ((row = tess_row_mask_next(pending, row)) >= 0)
	{
		int			partition = spill_partition(spill, hashes[row]);
		SpillPart  *part = &side->parts[partition];

		if (part->stamp == spill->stamp)
			continue;
		part->stamp = spill->stamp;
		if (part->resident || side->current[partition] == 0)
			side_add_chunk(state, side, partition);
		else
			side_flush(state, side, partition);
	}
}

/*
 * Append the rows of pending, hashed and keyed by batch_keys, as records
 * of the side's partitions: the payload is the NULL bits and a word per
 * column of the batch at children; a by-reference value is copied into
 * its partition's value chunks right after its row went in, so that the
 * values of a chunk's rows are written with it or before it, never with
 * the chunk before. Rows go on as the partitions' chunks fill; pending
 * ends empty. With nulls, the NULL bits of the columns are kept there.
 */
static void
side_append(TessHashJoinState *state, SpillSide *side, TessBatch *batch,
			TessRowMask *pending, const int *children, uint64 *nulls)
{
	JoinSpill  *spill = state->spill;
	int			nwords = tess_row_mask_word_count(pending->nrows);
	int			width = 1 + side->nwords;
	TessRowMask before = {pending->nrows, NULL};
	TessDatumColumn *columns = spill->columns;
	int			row = -1;

	spill_reserve(state);
	before.bits = spill->before_bits;
	while ((row = tess_row_mask_next(pending, row)) >= 0)
		spill->payload[row * width] = 0;
	for (int word = 0; word < side->nwords; word++)
	{
		child_column(batch, children[word], pending, TESS_COLUMN_FOR_PROJECTION,
					 &columns[word]);
		row = -1;
		while ((row = tess_row_mask_next(pending, row)) >= 0)
		{
			uint64	   *record = &spill->payload[row * width];

			if (columns[word].isnull[row])
			{
				record[0] |= UINT64CONST(1) << word;
				if (nulls != NULL)
					*nulls |= UINT64CONST(1) << word;
			}
			record[1 + word] = columns[word].isnull[row] || !side->byvals[word] ?
				0 : columns[word].values[row];
		}
	}
	for (;;)
	{
		memcpy(spill->before_bits, pending->bits, sizeof(uint64) * nwords);
		check(state, state->kernels->table_append_partitioned(&side->ref, side->current,
															  spill->npartitions,
															  spill->shift,
															  side->payload_size,
															  state->hashes, state->nkeys,
															  state->table_keys,
															  (const uint8 *) spill->payload,
															  pending, state->offsets,
															  &state->status));
		for (int word = 0; word < nwords; word++)
			spill->before_bits[word] &= ~pending->bits[word];
		row = -1;
		while ((row = tess_row_mask_next(&before, row)) >= 0)
		{
			int			partition = spill_partition(spill, state->hashes[row]);
			uint8	   *payload = NULL;

			side->parts[partition].rows++;
			for (int word = 0; word < side->nwords; word++)
			{
				if (side->byvals[word] || columns[word].isnull[row])
					continue;
				if (payload == NULL)
					check(state, state->kernels->table_payload(&side->ref,
															   state->offsets[row],
															   &payload,
															   &state->status));
				((uint64 *) payload)[1 + word] =
					side_store(side, partition, columns[word].values[row],
							   side->typlens[word]);
			}
		}
		if (tess_row_mask_count(pending) == 0)
			break;
		make_chunks(state, side, pending, state->hashes);
	}
	side_flush_queue(state, side);
}

/* Append the rows of an inner batch to the partitions. */
static void
insert_spill(TessHashJoinState *state, TessBatch *batch)
{
	JoinSpill  *spill = state->spill;
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask pending;
	int			count;

	reserve_rows(state, nrows);
	memset(state->valid_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->valid_bits};
	pending = (TessRowMask) {nrows, state->pending_bits};
	batch_keys(state, batch, state->inner_keys, state->inner_kinds, &valid);
	count = tess_row_mask_count(&valid);
	if (count == 0)
		return;
	check(state, state->kernels->bloom_add(spill->bloom, spill->bloom_words,
										   state->hashes, &valid, &state->status));
	memcpy(state->pending_bits, state->valid_bits, sizeof(uint64) * nwords);
	side_append(state, &spill->build, batch, &pending, spill->build_children,
				&state->null_columns);
	state->build_rows += count;
	spill->total_rows += count;
	state->counters[JOIN_BUILD_ROWS] += count;
	make_room(state, true);
}

/*
 * The build is over: the resident partitions' records make the table the
 * outer batches probe now, the others wait on disk and in their tails.
 */
static void
finish_spill_build(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	int			nchunks = 0;
	uint64		rows = 0;

	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		SpillPart  *part = &side->parts[partition];

		if (!part->resident)
			continue;
		state->counters[JOIN_RESIDENT]++;
		rows += part->rows;
		reserve_chunks(state, nchunks + part->nchunks);
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			state->chunk_bases[nchunks] = side->bases[part->chunks[chunk]];
			state->chunk_lens[nchunks] = side->lens[part->chunks[chunk]];
			nchunks++;
		}
	}
	reserve_chunks(state, Max(nchunks, 1));
	state->table.index = NULL;
	state->table.nchunks = nchunks;
	/* The chunks count with the side's memory; the index with the table's. */
	state->table_bytes = 0;
	state->build_rows = rows;
	state->value_bases = side->value_bases;
	state->nvalue_chunks = side->nvalues;
	index_table(state);
}

/* Free a partition's tail and value chunks of one side. */
static void
side_release(SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];

	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		int			index = part->chunks[chunk];

		if (side->bases[index] != NULL && index != 0)
		{
			pfree(side->bases[index]);
			side->bases[index] = NULL;
			side->bytes -= side->lens[index];
		}
	}
	part->nchunks = 0;
	side->current[partition] = 0;
	for (int index = 0; index < part->nvalues; index++)
	{
		int			number = part->values[index];

		if (side->value_bases[number] != NULL)
		{
			pfree(side->value_bases[number]);
			side->value_bases[number] = NULL;
			side->bytes -= side->value_allocated[number];
		}
	}
	part->nvalues = 0;
	part->bytes = 0;
	tess_spill_drop(side->file, partition);
}

/*
 * The outer child is done, and so are the resident partitions: their
 * memory goes, and the files become readable.
 */
static void
start_joining(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	spill->joining = true;
	spill->partition = -1;
	/* Every outer row is written or answered: the filter's work is done. */
	pfree(spill->bloom);
	spill->bloom = NULL;
	spill->bloom_words = 0;
	for (int partition = 0; partition < spill->npartitions; partition++)
		if (spill->build.parts[partition].resident)
			side_release(&spill->build, partition);
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->bloom = NULL;
	state->bloom_words = 0;
	tess_spill_finish(spill->build.file);
	tess_spill_finish(spill->probe.file);
}

/* Forget the joined partition: its files, tails and what was read back. */
static void
end_partition(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	int			partition = spill->partition;

	if (partition < 0 || partition >= spill->npartitions)
		return;
	if (spill->reader != NULL)
	{
		tess_spill_close(spill->reader);
		spill->reader = NULL;
	}
	if (spill->build_reader != NULL)
	{
		tess_spill_close(spill->build_reader);
		spill->build_reader = NULL;
	}
	if (spill->carried != NULL)
	{
		pfree(spill->carried);
		spill->carried = NULL;
	}
	if (spill->matched_rows != NULL)
	{
		pfree(spill->matched_rows);
		spill->matched_rows = NULL;
	}
	spill->multipass = false;
	spill->final_pass = false;
	for (int index = 0; index < spill->nloaded; index++)
		spill->build.value_bases[spill->loaded_values[index]] = NULL;
	spill->nloaded = 0;
	for (int index = 0; index < spill->nblock_values; index++)
		spill->probe.value_bases[spill->block_values[index]] = NULL;
	spill->nblock_values = 0;
	side_release(&spill->build, partition);
	side_release(&spill->probe, partition);
	MemoryContextReset(spill->part_context);
	MemoryContextReset(spill->block_context);
	spill->block = NULL;
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
}

/* A chunk of the joined partition's table, read back or its tail. */
static void
add_loaded_chunk(TessHashJoinState *state, void *base, Size len)
{
	int			chunk = state->table.nchunks;

	if (chunk == TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	reserve_chunks(state, chunk + 1);
	state->chunk_bases[chunk] = base;
	state->chunk_lens[chunk] = len;
	state->table.nchunks++;
}

/*
 * The bytes a piece of a partition's table may take: what hash_mem leaves
 * besides the rest of spilling and a chunk of outer rows with its values,
 * two thirds of it, the rest for the index.
 */
static Size
spill_room(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	Size		limit = get_hash_memory_limit();
	Size		used = spill_memory(spill, NULL) + 2 * spill->probe.chunk_len;

	return used >= limit ? 0 : (limit - used) / 3 * 2;
}

/* Forget the table of a piece, and the values read for it. */
static void
drop_piece(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	for (int index = 0; index < spill->nloaded; index++)
		spill->build.value_bases[spill->loaded_values[index]] = NULL;
	spill->nloaded = 0;
	MemoryContextReset(spill->part_context);
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	state->build_rows = 0;
	state->duplicates = 0;
}

/* A value chunk read back into the piece's table. */
static void
add_loaded_values(JoinSpill *spill, uint32 number, void *body)
{
	if (number >= (uint32) spill->build.nvalues)
		elog(ERROR, "TessHashJoin read back a value chunk it never wrote");
	spill->build.value_bases[number] = body;
	grow_ints(spill->context, &spill->loaded_values, &spill->loaded_slots,
			  spill->nloaded + 1);
	spill->loaded_values[spill->nloaded++] = number;
}

/*
 * The joined partition's next piece: its blocks read back, as many whole
 * groups as room allows (all of them when it does), and once the file is
 * done its tail, indexed as a table of its own.
 */
static Size spill_room(TessHashJoinState *state);

static void
load_piece(TessHashJoinState *state, int partition, bool whole)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	SpillPart  *part = &side->parts[partition];
	TessSpillHeader header;
	uint64		buckets = state->counters[JOIN_BUCKETS];
	Size		record = TYPEALIGN(8, 16 + 8 * side->nkeys + side->payload_size);
	Size		loaded = 0;
	bool		records = false;
	uint64		rows = 0;
	Size		room;
	bool		byref = false;

	for (int word = 0; word < side->nwords; word++)
		byref |= !side->byvals[word];
	drop_piece(state);
	room = whole ? SIZE_MAX : spill_room(state);
	if (spill->carried != NULL)
	{
		void	   *body = MemoryContextAllocExtended(spill->part_context,
													  Max(spill->carried_header.len, 8),
													  MCXT_ALLOC_HUGE);

		memcpy(body, spill->carried, spill->carried_header.len);
		pfree(spill->carried);
		spill->carried = NULL;
		loaded += spill->carried_header.len;
		if (spill->carried_header.kind == TESS_SPILL_VALUES)
			add_loaded_values(spill, spill->carried_header.number, body);
		else
		{
			add_loaded_chunk(state, body, spill->carried_header.len);
			rows += (spill->carried_header.len - TESS_TABLE_CHUNK_HEADER) / record;
			records = true;
		}
	}
	while (spill->build_reader != NULL &&
		   tess_spill_read_header(spill->build_reader, &header))
	{
		void	   *body;

		/*
		 * A group ends where values follow records, or, without values, at
		 * every chunk: the piece may end there.
		 */
		if (records && loaded >= room &&
			(header.kind == TESS_SPILL_VALUES || !byref))
		{
			spill->carried = MemoryContextAllocExtended(spill->context,
														Max(header.len, 8),
														MCXT_ALLOC_HUGE);
			spill->carried_header = header;
			tess_spill_read_body(spill->build_reader, spill->carried, header.len);
			break;
		}
		body = MemoryContextAllocExtended(spill->part_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		tess_spill_read_body(spill->build_reader, body, header.len);
		loaded += header.len;
		if (header.kind == TESS_SPILL_VALUES)
		{
			add_loaded_values(spill, header.number, body);
			records = false;
			continue;
		}
		add_loaded_chunk(state, body, header.len);
		rows += (header.len - TESS_TABLE_CHUNK_HEADER) / record;
		records = true;
	}
	if (spill->carried == NULL)
	{
		/* The file is done: the tail goes with the last piece. */
		if (spill->build_reader != NULL)
		{
			tess_spill_close(spill->build_reader);
			spill->build_reader = NULL;
		}
		spill->pieces_done = true;
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			int			index = part->chunks[chunk];
			Size		used = chunk_used(side->bases[index]);

			if (used > TESS_TABLE_CHUNK_HEADER)
			{
				add_loaded_chunk(state, side->bases[index], side->lens[index]);
				rows += (used - TESS_TABLE_CHUNK_HEADER) / record;
				state->counters[JOIN_TAILS]++;
			}
		}
	}
	state->build_rows = rows;
	state->duplicates = 0;
	state->bloom = NULL;
	state->bloom_words = 0;
	/* The rows written passed the Bloom filter: no other one pays. */
	state->bloom_decided = true;
	state->value_bases = side->value_bases;
	state->nvalue_chunks = side->nvalues;
	index_table(state);
	/* The buckets count for the build, not for each partition. */
	state->counters[JOIN_BUCKETS] = buckets;
	/* Duplicates the resident table did not have: the pairs go compact. */
	if (!state->compact && state->duplicates > 0)
		decide_compact(state);
}

/* Read the joined partition's outer rows from their first. */
static void
open_outer_rows(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	spill->reader = tess_spill_open(spill->probe.file, 0, spill->partition);
	spill->tail_read = false;
	spill->block = NULL;
	spill->ordinal = 0;
}

/*
 * The next pass over the joined partition's outer rows: with its next
 * piece, then, for a left or anti join, the last one without a table.
 * False when the partition is done.
 */
static bool
next_pass(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	if (!spill->multipass)
		return false;
	if (!spill->pieces_done)
		load_piece(state, spill->partition, false);
	else if ((state->jointype == JOIN_LEFT || state->jointype == JOIN_ANTI) &&
			 !spill->final_pass)
	{
		drop_piece(state);
		spill->final_pass = true;
	}
	else
		return false;
	state->counters[JOIN_PASSES]++;
	open_outer_rows(state);
	note_memory(state);
	return true;
}

/*
 * The next partition to join, with its table loaded: one with outer rows
 * written; the others are forgotten. False when none is left.
 */
static bool
next_partition(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	end_partition(state);
	for (int partition = spill->partition + 1; partition < spill->npartitions; partition++)
	{
		if (spill->build.parts[partition].resident)
			continue;
		if (spill->probe.parts[partition].rows == 0)
		{
			side_release(&spill->build, partition);
			side_release(&spill->probe, partition);
			continue;
		}
		spill->partition = partition;
		spill->build_reader = tess_spill_open(spill->build.file, 0, partition);
		spill->pieces_done = false;
		/* The file larger than the room: pieces, and passes over the outer rows. */
		spill->multipass = spill->build.parts[partition].disk_bytes > spill_room(state);
		if (spill->multipass && state->jointype != JOIN_INNER)
			spill->matched_rows =
				MemoryContextAllocExtended(spill->context,
										   sizeof(uint64) *
										   Max((spill->probe.parts[partition].rows + 63) / 64, 1),
										   MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
		load_piece(state, partition, !spill->multipass);
		open_outer_rows(state);
		note_memory(state);
		return true;
	}
	spill->partition = spill->npartitions;
	return false;
}

/*
 * The next batch of the joined partition's outer rows: up to
 * JOIN_COMPACT_ROWS records of the chunk being read, each stored column
 * gathered from their payload; the chunks come from the file, each after
 * its values, and then the tail. False when none is left.
 */
static bool
next_spilled(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->probe;

	for (;;)
	{
		TessSpillHeader header;
		int			count;

		if (spill->block != NULL)
		{
			check(state, state->kernels->table_scan(&spill->block_ref, &spill->cursor,
													spill->offsets, JOIN_COMPACT_ROWS,
													&count, &state->status));
			if (count > 0)
			{
				TessRowMask rows = {count, spill->bits};

				spill->bits[0] = count == 64 ? ~UINT64CONST(0) :
					(UINT64CONST(1) << count) - 1;
				spill->batch.rows.nrows = count;
				spill->batch_ordinal = spill->ordinal;
				spill->ordinal += count;
				check(state, state->kernels->table_gather(&spill->block_ref,
														  spill->offsets, &rows, 0,
														  (Datum *) spill->words,
														  &state->status));
				for (int word = 0; word < side->nwords; word++)
				{
					Datum	   *values = spill->values[word];
					bool	   *isnull = spill->isnull[word];

					check(state, state->kernels->table_gather(&spill->block_ref,
															  spill->offsets, &rows,
															  sizeof(uint64) * (1 + word),
															  values, &state->status));
					for (int row = 0; row < count; row++)
					{
						uint64		ref = DatumGetUInt64(values[row]);

						isnull[row] = (spill->words[row] >> word) & 1;
						if (isnull[row] || side->byvals[word])
							continue;
						if ((ref >> 32) - 1 >= (uint64) side->nvalues ||
							side->value_bases[(ref >> 32) - 1] == NULL)
							elog(ERROR, "TessHashJoin read back a value it did not keep");
						values[row] = PointerGetDatum(side->value_bases[(ref >> 32) - 1] +
													  (ref & 0xFFFFFFFF));
					}
				}
				return true;
			}
			spill->block = NULL;
		}
		/* The next chunk: the previous one and its values go. */
		for (int index = 0; index < spill->nblock_values; index++)
			side->value_bases[spill->block_values[index]] = NULL;
		spill->nblock_values = 0;
		MemoryContextReset(spill->block_context);
		if (spill->reader != NULL)
		{
			while (tess_spill_read_header(spill->reader, &header))
			{
				void	   *body = MemoryContextAllocExtended(spill->block_context,
															  Max(header.len, 8),
															  MCXT_ALLOC_HUGE);

				tess_spill_read_body(spill->reader, body, header.len);
				if (header.kind == TESS_SPILL_VALUES)
				{
					if (header.number >= (uint32) side->nvalues)
						elog(ERROR, "TessHashJoin read back a value chunk it never wrote");
					side->value_bases[header.number] = body;
					grow_ints(spill->context, &spill->block_values,
							  &spill->block_slots, spill->nblock_values + 1);
					spill->block_values[spill->nblock_values++] = header.number;
					continue;
				}
				spill->block = body;
				spill->block_len = header.len;
				break;
			}
			if (spill->block == NULL)
			{
				tess_spill_close(spill->reader);
				spill->reader = NULL;
			}
		}
		if (spill->block == NULL && !spill->tail_read)
		{
			int			index = side->current[spill->partition];

			spill->tail_read = true;
			if (index != 0 && chunk_used(side->bases[index]) > TESS_TABLE_CHUNK_HEADER)
			{
				spill->block = side->bases[index];
				spill->block_len = side->lens[index];
				state->counters[JOIN_TAILS]++;
			}
		}
		if (spill->block == NULL)
			return false;
		spill->block_base[0] = spill->block;
		spill->block_lens[0] = spill->block_len;
		spill->block_ref.index = side->ref.index;
		spill->block_ref.index_len = side->ref.index_len;
		spill->block_ref.chunks = spill->block_base;
		spill->block_ref.chunk_lens = spill->block_lens;
		spill->block_ref.nchunks = 1;
		spill->cursor = 0;
		note_memory(state);
	}
}

/* A column of a batch of outer rows read back: a stored column's values. */
static void
spill_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				 TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessHashJoinState *state = (TessHashJoinState *) batch->private_data;
	JoinSpill  *spill = state->spill;
	int			word;

	if (column < 0 || column >= spill->nchild || spill->word_of[column] == 0)
		elog(ERROR, "TessHashJoin did not keep outer column %d", column);
	word = spill->word_of[column] - 1;
	result->values = spill->values[word];
	result->isnull = spill->isnull[word];
	result->nrows = batch->rows.nrows;
}

/*
 * An outer batch, while the table spills: the valid rows of the
 * partitions on disk leave the batch's probe. Those of a partition with
 * no inner row, and those the filter of every inner row rejects, have no
 * pair and stay to be answered now; the others are written as records of
 * their partitions, leave the rows to answer and come back when their
 * partition is joined.
 */
static void
spill_outer(TessHashJoinState *state, TessBatch *batch, TessRowMask *valid)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->probe;
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask candidates = {nrows, state->pending_bits};
	TessRowMask pending = {nrows, state->next_bits};
	uint64		any = 0;

	for (int word = 0; word < nwords; word++)
	{
		uint64		bits = valid->bits[word];
		uint64		out = 0;
		uint64		written = 0;

		while (bits != 0)
		{
			int			bit = pg_rightmost_one_pos64(bits);
			int			partition = spill_partition(spill, state->hashes[word * 64 + bit]);

			bits &= bits - 1;
			if (spill->build.parts[partition].resident)
				continue;
			out |= UINT64CONST(1) << bit;
			if (spill->build.parts[partition].rows > 0)
				written |= UINT64CONST(1) << bit;
		}
		valid->bits[word] &= ~out;
		state->pending_bits[word] = written;
		any |= written;
	}
	if (any == 0)
		return;
	/*
	 * The filter of every inner row: a row it rejects has no pair anywhere.
	 * The kernel fills the mask whole, but checks it is a mask of nrows.
	 */
	memset(state->next_bits, 0, sizeof(uint64) * nwords);
	check(state, state->kernels->bloom_probe(spill->bloom, spill->bloom_words,
											 state->hashes, &candidates, &pending,
											 &state->status));
	state->counters[JOIN_BLOOM_REMOVED] +=
		tess_row_mask_count(&candidates) - tess_row_mask_count(&pending);
	if (tess_row_mask_count(&pending) == 0)
		return;
	for (int word = 0; word < nwords; word++)
		state->active_bits[word] &= ~state->next_bits[word];

	side_append(state, side, batch, &pending, spill->stored, NULL);
	make_room(state, false);
}

/* The bits of count outer rows of the joined partition from an ordinal. */
static uint64
matched_word(const JoinSpill *spill, int count)
{
	uint64		word = 0;

	for (int row = 0; row < count; row++)
	{
		uint64		at = spill->batch_ordinal + row;

		word |= ((spill->matched_rows[at / 64] >> (at % 64)) & 1) << row;
	}
	return word;
}

/*
 * The next outer batch: the outer child's, while it has one, then those
 * of the partitions on disk, each joined in turn. The rows to answer are
 * the batch's selected ones, until the table sends some to disk.
 */
static TessBatch *
outer_next(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	TessBatch  *batch;

	if (spill == NULL || !spill->joining)
	{
		batch = spill != NULL && spill->child_done ? NULL :
			tess_input_next(state->outer_input);
		if (batch != NULL)
		{
			reserve_rows(state, batch->rows.nrows);
			memcpy(state->active_bits, batch->rows.bits,
				   sizeof(uint64) * tess_row_mask_word_count(batch->rows.nrows));
			state->counters[JOIN_PROBE_ROWS] += tess_row_mask_count(&batch->rows);
			return batch;
		}
		if (spill == NULL)
			return NULL;
		/* The resident table goes: not while a compact batch holds its pairs. */
		spill->child_done = true;
		if (spill->holding)
			return NULL;
		start_joining(state);
	}
	for (;;)
	{
		if (spill->partition >= 0 && spill->partition < spill->npartitions &&
			next_spilled(state))
		{
			uint64		active = spill->bits[0];

			/*
			 * Passes over the outer rows: left and anti joins answer the rows
			 * without a pair in the last one only.
			 */
			if (spill->multipass &&
				(state->jointype == JOIN_LEFT || state->jointype == JOIN_ANTI))
				active = spill->final_pass ?
					active & ~matched_word(spill, spill->batch.rows.nrows) : 0;
			reserve_rows(state, spill->batch.rows.nrows);
			state->active_bits[0] = active;
			return &spill->batch;
		}
		if (spill->partition >= spill->npartitions || spill->holding)
			return NULL;
		if (spill->partition >= 0 && next_pass(state))
			continue;
		if (!next_partition(state))
			return NULL;
	}
}

/*
 * Finish an outer batch: the child's goes back to it; one read back notes
 * the rows that found a pair, when the partition is joined in passes.
 */
static void
outer_finish(TessHashJoinState *state, TessBatch *batch)
{
	JoinSpill  *spill = state->spill;

	if (spill == NULL || batch != &spill->batch)
	{
		tess_input_finish(state->outer_input);
		return;
	}
	if (spill->matched_rows != NULL && !spill->final_pass)
	{
		uint64		bits = state->matched_bits[0];

		while (bits != 0)
		{
			uint64		at = spill->batch_ordinal + pg_rightmost_one_pos64(bits);

			bits &= bits - 1;
			spill->matched_rows[at / 64] |= UINT64CONST(1) << (at % 64);
		}
	}
}

/* Delete the files and free the memory of spilling. */
static void
spill_free(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	if (spill == NULL)
		return;
	if (spill->reader != NULL)
		tess_spill_close(spill->reader);
	tess_spill_free(spill->build.file);
	tess_spill_free(spill->probe.file);
	MemoryContextDelete(spill->context);
	pfree(spill);
	state->spill = NULL;
	state->value_bases = NULL;
	state->value_slots = 0;
	state->nvalue_chunks = 0;
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->table_bytes = 0;
	state->bloom = NULL;
	state->bloom_words = 0;
}

/*
 * The next record of each row's key: one step in a table grouped by
 * insertion, a walk down the chain in a shared table, whose participants
 * inserted without grouping.
 */
static TessStatusCode
next_record(TessHashJoinState *state, const TessRowMask *rows, TessRowMask *found)
{
	if (state->shared != NULL)
		return state->kernels->table_next_match(&state->table, state->offsets,
												rows, found, &state->status);
	return state->kernels->table_next_in_group(&state->table, state->offsets,
											   rows, found, &state->status);
}

/*
 * The query's dynamic shared memory, where a shared build keeps its
 * table: the Gather installs it only while it runs the plan, so it is
 * kept for leaving the build and freeing the table at shutdown, as the
 * core's parallel hash join keeps it.
 */
static dsa_area *
query_dsa(TessHashJoinState *state)
{
	dsa_area   *area = state->css.ss.ps.state->es_query_dsa;

	if (area != NULL)
		state->area = area;
	if (state->area == NULL)
		elog(ERROR, "TessHashJoin found no shared memory for its shared table");
	return state->area;
}

/*
 * The shared table as the elected participant published it: its index,
 * and the bases of its chunks in this process, from the directory.
 */
static void
attach_shared_table(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);
	int			nchunks = state->shared->nchunks;
	dsa_pointer *bases;
	Size	   *lens;

	/* SIZE published both before the barrier that led here. */
	Assert(DsaPointerIsValid(state->shared->index));
	Assert(DsaPointerIsValid(state->shared->directory));
	state->table.index = dsa_get_address(area, state->shared->index);
	state->table.index_len = state->shared->index_len;
	reserve_chunks(state, Max(nchunks, 1));
	bases = dsa_get_address(area, state->shared->directory);
	lens = (Size *) (bases + nchunks);
	for (int chunk = 0; chunk < nchunks; chunk++)
	{
		state->chunk_bases[chunk] = dsa_get_address(area, bases[chunk]);
		state->chunk_lens[chunk] = lens[chunk];
	}
	state->table.nchunks = nchunks;
	/* Every participant's value chunks, which a gather reads. */
	reserve_values(state, Max(state->shared->nvalue_chunks, 1));
	bases = dsa_get_address(area, state->shared->value_directory);
	for (int chunk = 0; chunk < state->shared->nvalue_chunks; chunk++)
		state->value_bases[chunk] = dsa_get_address(area, bases[chunk]);
	state->nvalue_chunks = state->shared->nvalue_chunks;
}

/*
 * A cleared shared Bloom filter for a table of `records` records, in
 * place of the one before; only the elected participant, whom the build
 * barrier separates from the probes, calls it.
 */
static void
allocate_shared_filter(TessHashJoinState *state, uint64 records)
{
	dsa_area   *area = query_dsa(state);
	Size		nwords;

	if (DsaPointerIsValid(state->shared->filter))
		dsa_free(area, state->shared->filter);
	check(state, state->kernels->bloom_shared_words(records, &nwords, &state->status));
	state->shared->filter = dsa_allocate_extended(area, mul_size(sizeof(uint64), nwords),
												  DSA_ALLOC_HUGE);
	state->shared->filter_words = nwords;
	check(state, state->kernels->bloom_shared_init(dsa_get_address(area,
																   state->shared->filter),
												   nwords, &state->status));
}

/* The memory a shared build's elected participant holds, in bytes. */
static void
note_shared_memory(TessHashJoinState *state, Size bytes)
{
	bytes = add_size(bytes, mul_size(sizeof(uint64), state->shared->filter_words));
	state->peak_memory = Max(state->peak_memory, bytes);
}

/*
 * BUILD: another chunk of this participant's, the last one being full:
 * numbered by the build counters and entered in the table's list.
 */
static void
add_own_chunk(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);
	Size		len = state->nown == 0 ? JOIN_FIRST_CHUNK : JOIN_CHUNK_LEN;
	uint64		number;
	dsa_pointer block;
	JoinChunk  *header;

	check(state, state->kernels->build_take_chunk(state->shared->counters, &number,
												  &state->status));
	if (number >= TESS_TABLE_MAX_CHUNKS)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
						TESS_TABLE_MAX_CHUNKS)));
	if (state->nown == state->own_slots)
	{
		MemoryContext context = state->css.ss.ps.state->es_query_cxt;
		int			slots = Max(state->own_slots * 2, 8);

		state->own_chunks = state->own_chunks == NULL ?
			MemoryContextAlloc(context, sizeof(int) * slots) :
			repalloc(state->own_chunks, sizeof(int) * slots);
		state->own_slots = slots;
	}
	block = dsa_allocate_extended(area, JOIN_CHUNK_HEADER + len, DSA_ALLOC_HUGE);
	header = dsa_get_address(area, block);
	header->number = number;
	header->len = len;
	state->own_base = (char *) header + JOIN_CHUNK_HEADER;
	state->own_len = len;
	check(state, state->kernels->table_chunk_init(state->own_base, len,
												  &state->status));
	SpinLockAcquire(&state->shared->lock);
	header->next = state->shared->chunks;
	state->shared->chunks = block;
	SpinLockRelease(&state->shared->lock);
	state->own_chunks[state->nown++] = (int) number;
	state->own_bytes += JOIN_CHUNK_HEADER + len;
	state->counters[JOIN_CHUNKS]++;
}

/*
 * BUILD: append the rows of one inner batch to this participant's
 * chunks, adding chunks until they fit. The index is not made yet: the
 * table seen here is the one chunk appended to.
 */
static void
insert_shared_batch(TessHashJoinState *state, TessBatch *batch)
{
	TessRowMask pending;
	int			count = prepare_inner(state, batch, &pending);
	bool		fresh = false;

	if (count == 0)
		return;
	if (state->nown == 0)
	{
		add_own_chunk(state);
		fresh = true;
	}
	for (;;)
	{
		TessTableRef own = {NULL, 0, &state->own_base, &state->own_len, 1};
		bool		appended = append_rows(state, &own, 0, &pending);

		if (tess_row_mask_count(&pending) == 0)
			break;
		if (!appended && fresh)
			elog(ERROR, "TessHashJoin cannot fit a row of its table in a chunk");
		add_own_chunk(state);
		fresh = true;
	}
	state->appended += count;
	state->build_rows += count;
	state->counters[JOIN_BUILD_ROWS] += count;
	state->peak_memory = Max(state->peak_memory,
							 add_size(state->own_bytes, state->value_bytes));
}

/* BUILD: this participant's share of the inner side, then its report. */
static void
build_shared_inner(TessHashJoinState *state)
{
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->inner_input);

		if (batch == NULL)
			break;
		if (tess_row_mask_count(&batch->rows) > 0)
			insert_shared_batch(state, batch);
		tess_input_finish(state->inner_input);
	}
	check(state, state->kernels->build_report(state->shared->counters,
											  state->appended,
											  state->null_columns,
											  &state->status));
}

/*
 * SIZE: the index for exactly the records appended, the directory of the
 * chunks by number, and the filter, as the elected participant.
 */
static void
size_shared_table(TessHashJoinState *state)
{
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	dsa_area   *area = query_dsa(state);
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	uint64		records;
	uint64		nulls;
	uint64		nchunks;
	uint64		capacity;
	dsa_pointer *bases;
	Size	   *lens;
	dsa_pointer block;
	Size		size;

	/* The elected one alone, once per build. */
	Assert(!DsaPointerIsValid(state->shared->index));
	Assert(!DsaPointerIsValid(state->shared->directory));
	check(state, state->kernels->build_totals(state->shared->counters, &records,
											  &nulls, &nchunks, NULL,
											  &state->status));
	capacity = Max(records, JOIN_INITIAL_ROWS);
	check(state, state->kernels->table_size(state->nkeys, state->inner_kinds,
											payload_size, capacity,
											&size, &state->status));
	state->shared->index = dsa_allocate_extended(area, size, DSA_ALLOC_HUGE);
	state->shared->index_len = size;
	check(state, state->kernels->table_create(dsa_get_address(area, state->shared->index),
											  size, state->nkeys, state->inner_kinds,
											  payload_size, capacity,
											  &state->status));
	/* The directory: every chunk under the number it took, none left out. */
	state->shared->directory =
		dsa_allocate_extended(area,
							  mul_size(Max(nchunks, 1), sizeof(dsa_pointer) + sizeof(Size)),
							  DSA_ALLOC_ZERO);
	bases = dsa_get_address(area, state->shared->directory);
	lens = (Size *) (bases + nchunks);
	for (block = state->shared->chunks; DsaPointerIsValid(block);)
	{
		JoinChunk  *header = dsa_get_address(area, block);

		if (header->number >= nchunks || lens[header->number] != 0)
			elog(ERROR, "TessHashJoin found chunk %llu out of the directory",
				 (unsigned long long) header->number);
		bases[header->number] = block + JOIN_CHUNK_HEADER;
		lens[header->number] = header->len;
		block = header->next;
	}
	for (uint64 chunk = 0; chunk < nchunks; chunk++)
		if (lens[chunk] == 0)
			elog(ERROR, "TessHashJoin is missing chunk %llu of its table",
				 (unsigned long long) chunk);
	state->shared->nchunks = (int) nchunks;
	/* The value chunks' directory: dsa_pointers of their bases by number. */
	state->shared->nvalue_chunks = (int) state->shared->next_value_chunk;
	state->shared->value_directory =
		dsa_allocate_extended(area,
							  mul_size(Max(state->shared->nvalue_chunks, 1),
									   sizeof(dsa_pointer)),
							  DSA_ALLOC_ZERO | DSA_ALLOC_HUGE);
	bases = dsa_get_address(area, state->shared->value_directory);
	for (block = state->shared->values; DsaPointerIsValid(block);)
	{
		JoinChunk  *header = dsa_get_address(area, block);

		if (header->number >= (uint64) state->shared->nvalue_chunks ||
			DsaPointerIsValid(bases[header->number]))
			elog(ERROR, "TessHashJoin found value chunk %llu out of the directory",
				 (unsigned long long) header->number);
		bases[header->number] = block + JOIN_CHUNK_HEADER;
		block = header->next;
	}
	for (int chunk = 0; chunk < state->shared->nvalue_chunks; chunk++)
		if (!DsaPointerIsValid(bases[chunk]))
			elog(ERROR, "TessHashJoin is missing value chunk %d of its table", chunk);
	attach_shared_table(state);
	check(state, state->kernels->table_stats(&state->table, &stats,
											 &state->status));
	allocate_shared_filter(state, capacity);
	note_shared_memory(state, add_size(size, add_size(state->own_bytes,
													  state->value_bytes)));
	state->counters[JOIN_BUILDS]++;
	state->counters[JOIN_BUCKETS] += stats.buckets;
}

/*
 * LINK: this participant's own chunks into the index, counting the
 * records whose keys the table held already unless the planner knows the
 * inner side unique: the probes then skip the rounds a table without
 * duplicates has no use for.
 */
static void
link_own_chunks(TessHashJoinState *state)
{
	uint64		duplicates = 0;

	attach_shared_table(state);
	for (int own = 0; own < state->nown; own++)
	{
		Size		from = TESS_TABLE_CHUNK_HEADER;
		uint64		repeated = 0;

		Assert(state->own_chunks[own] < state->table.nchunks);

		check(state, state->kernels->table_link(&state->table, state->own_chunks[own],
												&from, NULL,
												state->inner_unique ? NULL : &repeated,
												&state->status));
		duplicates += repeated;
	}
	if (duplicates > 0)
		check(state, state->kernels->build_add_duplicates(state->shared->counters,
														  duplicates,
														  &state->status));
	state->nown = 0;
}

/* Free the shared table, as the last participant to leave or at a rescan. */
static void
free_shared_table(TessHashJoinState *state)
{
	if (DsaPointerIsValid(state->shared->index))
	{
		dsa_free(query_dsa(state), state->shared->index);
		state->shared->index = InvalidDsaPointer;
		state->shared->index_len = 0;
	}
	if (DsaPointerIsValid(state->shared->directory))
	{
		dsa_free(query_dsa(state), state->shared->directory);
		state->shared->directory = InvalidDsaPointer;
		state->shared->nchunks = 0;
	}
	while (DsaPointerIsValid(state->shared->chunks))
	{
		dsa_pointer block = state->shared->chunks;

		state->shared->chunks =
			((JoinChunk *) dsa_get_address(query_dsa(state), block))->next;
		dsa_free(query_dsa(state), block);
	}
	while (DsaPointerIsValid(state->shared->values))
	{
		dsa_pointer block = state->shared->values;

		state->shared->values =
			((JoinChunk *) dsa_get_address(query_dsa(state), block))->next;
		dsa_free(query_dsa(state), block);
	}
	if (DsaPointerIsValid(state->shared->value_directory))
	{
		dsa_free(query_dsa(state), state->shared->value_directory);
		state->shared->value_directory = InvalidDsaPointer;
	}
	state->shared->next_value_chunk = 0;
	state->shared->nvalue_chunks = 0;
	reset_values(state);
	if (DsaPointerIsValid(state->shared->filter))
	{
		dsa_free(query_dsa(state), state->shared->filter);
		state->shared->filter = InvalidDsaPointer;
		state->shared->filter_words = 0;
	}
	take_back_bloom(state);
	state->bloom = NULL;
	state->bloom_words = 0;
	state->table.index = NULL;
	state->table.index_len = 0;
	state->table.nchunks = 0;
}

/*
 * Step through the shared build's phases until probing starts: the node
 * performs each action the participant returns, and the build barrier's
 * waits stay here, since they may raise an error.
 */
static void
build_shared(TessHashJoinState *state)
{
	uint32		reply = 0;

	state->compact_decided = false;
	reset_values(state);
	state->build_rows = 0;
	state->duplicates = 0;
	state->null_columns = 0;
	state->own_base = NULL;
	state->own_len = 0;
	state->nown = 0;
	state->own_bytes = 0;
	state->appended = 0;
	state->participating = true;
	/* This build's filter: decided again by this participant's batches. */
	take_back_bloom(state);
	state->bloom = NULL;
	state->bloom_words = 0;
	state->bloom_decided = false;
	state->bloom_shared = false;
	state->bloom_ready = false;
	state->sample_rows = 0;
	state->sample_found = 0;
	for (;;)
	{
		uint32		action;

		check(state, state->kernels->build_step(&state->participant,
												state->shared->counters, reply,
												&action, &state->status));
		reply = 0;
		switch (action)
		{
			case TESS_BUILD_ATTACH:
				reply = BarrierAttach(&state->shared->build);
				break;
			case TESS_BUILD_ARRIVE_AND_WAIT:
				reply = BarrierArriveAndWait(&state->shared->build,
											 PG_WAIT_EXTENSION) ? 1 : 0;
				break;
			case TESS_BUILD_DO_BUILD:
				Assert(BarrierPhase(&state->shared->build) == TESS_BUILD_BUILD);
				build_shared_inner(state);
				break;
			case TESS_BUILD_DO_SIZE:
				Assert(BarrierPhase(&state->shared->build) == TESS_BUILD_SIZE);
				size_shared_table(state);
				break;
			case TESS_BUILD_DO_LINK:
				Assert(BarrierPhase(&state->shared->build) == TESS_BUILD_LINK);
				link_own_chunks(state);
				break;
			case TESS_BUILD_DO_PROBE:
				Assert(BarrierPhase(&state->shared->build) == TESS_BUILD_PROBE);
				{
					uint64		records;
					uint64		nchunks;

					attach_shared_table(state);
					/* The whole table's rows and duplicates, every link done. */
					check(state, state->kernels->build_totals(state->shared->counters,
															  &records,
															  &state->null_columns,
															  &nchunks,
															  &state->duplicates,
															  &state->status));
					state->build_rows = records;
					state->built = true;
					return;
				}
			case TESS_BUILD_DETACH:
				BarrierDetach(&state->shared->build);
				break;
			case TESS_BUILD_DONE:
				/* Attached after the last one left: nothing is left to probe. */
				state->participating = false;
				state->build_rows = 0;
				state->built = true;
				state->done = true;
				return;
			default:
				elog(ERROR, "TessHashJoin got build action %u out of order", action);
		}
	}
}

/* Leave a shared build after probing; the last one to leave frees the table. */
static void
leave_shared(TessHashJoinState *state)
{
	uint32		reply = 0;

	if (!state->participating)
		return;
	for (;;)
	{
		uint32		action;

		check(state, state->kernels->build_step(&state->participant,
												state->shared->counters, reply,
												&action, &state->status));
		reply = 0;
		switch (action)
		{
			case TESS_BUILD_ARRIVE_AND_DETACH:
				reply = BarrierArriveAndDetach(&state->shared->build) ? 1 : 0;
				break;
			case TESS_BUILD_DO_FREE:
				free_shared_table(state);
				state->participating = false;
				return;
			case TESS_BUILD_DONE:
				state->participating = false;
				state->table.index = NULL;
				state->table.index_len = 0;
				state->table.nchunks = 0;
				return;
			default:
				elog(ERROR, "TessHashJoin got build action %u out of order", action);
		}
	}
}

/*
 * The inner column kept in payload word `word`, for the rows of the
 * round: the records' NULL bits once per round, then the column's word.
 * A parent asks with a subset of the round's rows, which the whole round
 * covers.
 */
static void
gather_inner(TessHashJoinState *state, int word)
{
	TessRowMask round = {state->batch.rows.nrows, state->current_bits};
	bool		nullable = (state->null_columns >> word) & 1;
	int			row = -1;

	if (state->gathered[word])
		return;
	/* A column no inner row left NULL keeps its flags false. */
	if (nullable && !state->nulls_gathered)
	{
		check(state, state->kernels->table_gather(&state->table,
												  state->current_offsets, &round, 0,
												  state->null_words,
												  &state->status));
		state->nulls_gathered = true;
	}
	check(state, state->kernels->table_gather(&state->table,
											  state->current_offsets, &round,
											  sizeof(uint64) * (1 + word),
											  state->inner_values[word],
											  &state->status));
	/* A by-reference value's word is its reference: its address here. */
	if (!state->typbyvals[state->payload_columns[word]])
	{
		Datum	   *values = state->inner_values[word];

		while ((row = tess_row_mask_next(&round, row)) >= 0)
		{
			uint64		ref = DatumGetUInt64(values[row]);

			if (ref == 0)
				continue;
			Assert((ref >> 32) - 1 < (uint64) state->nvalue_chunks);
			values[row] = PointerGetDatum(state->value_bases[(ref >> 32) - 1] +
										  (ref & 0xFFFFFFFF));
		}
		row = -1;
	}
	if (nullable)
		while ((row = tess_row_mask_next(&round, row)) >= 0)
			state->inner_isnull[word][row] =
				(DatumGetUInt64(state->null_words[row]) >> word) & 1;
	state->gathered[word] = true;
}

/*
 * A column of the published batch: an outer column is the outer batch's
 * own, an inner one is gathered from the round's records.
 */
static void
join_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				TessColumnPurpose purpose, TessDatumColumn *result)
{
	TessHashJoinState *state = (TessHashJoinState *) batch->private_data;
	int			word;

	if (column < 0 || column >= state->ncolumns)
		elog(ERROR, "TessHashJoin has no column %d", column);
	if (state->sides[column] == JOIN_SIDE_OUTER && state->output_compact)
	{
		if (state->compact_values[column] == NULL)
			elog(ERROR, "TessHashJoin column %d was not requested", column);
		result->values = state->compact_values[column];
		result->isnull = state->compact_isnull[column];
		result->nrows = batch->rows.nrows;
		return;
	}
	if (state->sides[column] == JOIN_SIDE_OUTER)
	{
		TessBatch  *outer = state->outer_batch;

		outer->ops->get_datum_column(outer, state->child_columns[column], rows,
									 purpose, result);
		return;
	}
	word = state->payload_words[column];
	if (word == 0)
		elog(ERROR, "TessHashJoin column %d was not requested", column);
	/* LEFT: the rows without a match have NULL inner columns. */
	if (state->null_round)
	{
		result->values = state->null_values;
		result->isnull = state->null_isnull;
		result->nrows = batch->rows.nrows;
		return;
	}
	gather_inner(state, word - 1);
	result->values = state->inner_values[word - 1];
	result->isnull = state->inner_isnull[word - 1];
	result->nrows = batch->rows.nrows;
}

static const TessBatchOps join_batch_ops = {
	TESS_ABI_INITIALIZER(TESS_BATCH_OPS_ABI_VERSION, TessBatchOps),
	.get_datum_column = join_get_column,
};

/* Make the round's rows the published batch's selection. */
static void
start_round(TessHashJoinState *state)
{
	int			nrows = state->outer_batch->rows.nrows;
	TessRowMask round = {nrows, state->round_bits};

	memcpy(state->published_bits, state->round_bits,
		   sizeof(uint64) * tess_row_mask_word_count(nrows));
	state->batch.rows.nrows = nrows;
	state->batch.rows.bits = state->published_bits;
	state->current_offsets = state->offsets;
	state->current_bits = state->round_bits;
	state->nulls_gathered = false;
	memset(state->gathered, 0, sizeof(bool) * Max(state->npayload, 1));
	state->counters[JOIN_MATCHES] += tess_row_mask_count(&round);
}

/*
 * LEFT: the selected rows of the outer batch without a match, published
 * with NULL inner columns. False when every row had one.
 */
static bool
start_null_round(TessHashJoinState *state)
{
	TessBatch  *outer = state->outer_batch;
	int			nrows = outer->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	uint64		any = 0;

	/* The rows still to answer: those written to disk go with their partition. */
	for (int word = 0; word < nwords; word++)
	{
		state->published_bits[word] = state->active_bits[word] &
			~state->matched_bits[word];
		any |= state->published_bits[word];
	}
	if (any == 0)
		return false;
	state->null_round = true;
	state->batch.rows.nrows = nrows;
	state->batch.rows.bits = state->published_bits;
	state->current_offsets = state->offsets;
	state->current_bits = state->published_bits;
	state->nulls_gathered = false;
	memset(state->gathered, 0, sizeof(bool) * Max(state->npayload, 1));
	return true;
}

/*
 * Offer the filter to the outer child, which may check its rows against it
 * before its costlier work (TessFilter's row-wise clauses): only when a
 * row without a pair leaves the join's output, as INNER and SEMI drop it.
 * A child that takes it passes only the rows the filter lets through, and
 * the join checks no more.
 */
static void
hand_down_bloom(TessHashJoinState *state)
{
	TessKeyFilter filter = TESS_STRUCT_INITIALIZER(TessKeyFilter);

	/* A table that spills: the filter knows only the resident rows. */
	if ((state->jointype != JOIN_INNER && state->jointype != JOIN_SEMI) ||
		state->spill != NULL)
		return;
	filter.nkeys = state->nkeys;
	filter.columns = state->outer_keys;
	filter.kinds = state->outer_kinds;
	filter.words = state->bloom;
	filter.nwords = state->bloom_words;
	filter.shared = state->bloom_shared;
	state->bloom_below = tess_input_set_key_filter(state->outer_input, &filter);
	if (state->bloom_below)
		state->counters[JOIN_BLOOM_BELOW]++;
}

/*
 * After the first JOIN_BLOOM_SAMPLE valid probe rows, a Bloom filter of
 * the table's keys when most of them found no record and the table is
 * past the cache: from then on a row the filter rejects skips the table.
 */
static void
decide_bloom(TessHashJoinState *state, uint64 rows, uint64 found)
{
	double		ratio = tess_join_bloom_ratio;

	state->sample_rows += rows;
	state->sample_found += found;
	/* At 1 the filter comes at once, whatever the sizes; at 0 never. */
	if (ratio < 1.0 && state->sample_rows < JOIN_BLOOM_SAMPLE)
		return;
	state->bloom_decided = true;
	if (ratio <= 0.0 ||
		(ratio < 1.0 &&
		 ((double) state->sample_found >= ratio * state->sample_rows ||
		  state->build_rows < JOIN_BLOOM_MIN_ROWS)))
		return;
	/*
	 * A shared table's filter: the first participant that wants it builds
	 * it for all; until it is ready, the others probe without it.
	 */
	if (state->shared != NULL)
	{
		bool		built;

		state->bloom = dsa_get_address(query_dsa(state), state->shared->filter);
		state->bloom_words = state->shared->filter_words;
		state->bloom_shared = true;
		check(state, state->kernels->table_try_build_bloom(&state->table,
														   state->bloom,
														   state->bloom_words,
														   &built, &state->status));
		if (built)
			state->counters[JOIN_BLOOM_FILTERS]++;
		hand_down_bloom(state);
		return;
	}
	check(state, state->kernels->table_bloom_words(state->build_rows,
												   &state->bloom_words,
												   &state->status));
	state->bloom = MemoryContextAllocExtended(state->table_context,
											  mul_size(sizeof(uint64),
													   state->bloom_words),
											  MCXT_ALLOC_HUGE);
	check(state, state->kernels->table_bloom(&state->table,
											 state->bloom, state->bloom_words,
											 &state->status));
	state->counters[JOIN_BLOOM_FILTERS]++;
	note_memory(state);
	hand_down_bloom(state);
}

/*
 * Probe the table with one outer batch: the rows whose key found a record
 * become the first round, after the Bloom filter, when there is one, let
 * them through. False when none did.
 */
static bool
probe_batch(TessHashJoinState *state, TessBatch *batch)
{
	int			nrows = batch->rows.nrows;
	int			nwords = tess_row_mask_word_count(nrows);
	TessRowMask valid;
	TessRowMask found;
	TessRowMask passed;
	int			count;
	int			matches;

	reserve_rows(state, nrows);
	/* A shorter batch than the last: no bits past its rows may remain. */
	memset(state->valid_bits, 0, sizeof(uint64) * nwords);
	memset(state->round_bits, 0, sizeof(uint64) * nwords);
	valid = (TessRowMask) {nrows, state->valid_bits};
	found = (TessRowMask) {nrows, state->round_bits};

	batch_keys(state, batch, state->outer_keys, state->outer_kinds, &valid);
	count = tess_row_mask_count(&valid);
	if (count == 0)
		return false;
	/* The rows of the partitions on disk wait for their partition. */
	if (state->spill != NULL && !state->spill->joining)
	{
		spill_outer(state, batch, &valid);
		count = tess_row_mask_count(&valid);
		if (count == 0)
			return false;
	}
	/*
	 * Passes over a partition's outer rows: the last has no table, and a
	 * semi or anti join's row that found a pair in an earlier one is done.
	 */
	if (state->spill != NULL && state->spill->multipass &&
		batch == &state->spill->batch)
	{
		if (state->spill->final_pass)
			return false;
		if (state->jointype == JOIN_SEMI || state->jointype == JOIN_ANTI)
		{
			valid.bits[0] &= ~matched_word(state->spill, nrows);
			count = tess_row_mask_count(&valid);
			if (count == 0)
				return false;
		}
	}
	if (state->bloom != NULL && !state->bloom_below && state->bloom_shared &&
		!state->bloom_ready)
		check(state, state->kernels->bloom_shared_ready(state->bloom, state->bloom_words,
														&state->bloom_ready,
														&state->status));
	if (state->bloom != NULL && !state->bloom_below &&
		(!state->bloom_shared || state->bloom_ready))
	{
		int			through;

		/* The kernel fills the mask whole, but checks it is a mask of nrows. */
		memset(state->pending_bits, 0, sizeof(uint64) * nwords);
		passed = (TessRowMask) {nrows, state->pending_bits};
		if (state->bloom_shared)
			check(state, state->kernels->bloom_shared_probe(state->bloom,
															state->bloom_words,
															state->hashes, &valid,
															&passed, &state->status));
		else
			check(state, state->kernels->bloom_probe(state->bloom, state->bloom_words,
													 state->hashes, &valid, &passed,
													 &state->status));
		through = tess_row_mask_count(&passed);
		state->counters[JOIN_BLOOM_REMOVED] += count - through;
		if (through == 0)
			return false;
		valid = passed;
	}
	check(state, state->kernels->table_probe(&state->table,
											 state->hashes, state->nkeys,
											 state->table_keys, &valid,
											 state->offsets, &found,
											 &state->status));
	matches = tess_row_mask_count(&found);
	if (!state->bloom_decided)
		decide_bloom(state, count, matches);
	return matches > 0;
}

/*
 * The next round: the next record of each row of the current round, as
 * long as some row has one; then the first round of the next outer batch
 * that matches. False at the end of the outer input.
 */
static bool
next_round(TessHashJoinState *state)
{
	for (;;)
	{
		TessBatch  *batch;
		bool		found;

		if (state->outer_batch != NULL && !state->null_round)
		{
			int			nrows = state->outer_batch->rows.nrows;

			/*
			 * Without duplicates no row has a next record; the last pass over
			 * a partition's outer rows has no table.
			 */
			if (!state->inner_unique && state->duplicates > 0 &&
				state->table.index != NULL)
			{
				TessRowMask rows = {nrows, state->next_bits};
				TessRowMask found = {nrows, state->round_bits};

				memcpy(state->next_bits, state->round_bits,
					   sizeof(uint64) * tess_row_mask_word_count(nrows));
				check(state, next_record(state, &rows, &found));
				if (tess_row_mask_count(&found) > 0)
				{
					start_round(state);
					return true;
				}
			}
			/* LEFT: after the pairs, the rows that had none. */
			if (state->jointype == JOIN_LEFT && start_null_round(state))
				return true;
		}
		if (state->outer_batch != NULL)
		{
			state->null_round = false;
			outer_finish(state, state->outer_batch);
			state->outer_batch = NULL;
		}
		batch = outer_next(state);
		if (batch == NULL)
			return false;
		found = probe_batch(state, batch);
		if (state->jointype == JOIN_LEFT)
		{
			int			nwords = tess_row_mask_word_count(batch->rows.nrows);

			/* Without join clauses every row found has its match. */
			state->outer_batch = batch;
			if (found && state->qual == NULL)
				memcpy(state->matched_bits, state->round_bits, sizeof(uint64) * nwords);
			else
				memset(state->matched_bits, 0, sizeof(uint64) * nwords);
			if (found)
			{
				start_round(state);
				return true;
			}
			if (start_null_round(state))
				return true;
			continue;
		}
		if (!found)
		{
			outer_finish(state, batch);
			continue;
		}
		state->outer_batch = batch;
		start_round(state);
		return true;
	}
}

/*
 * Fill a compact batch with the pairs of the rounds, from where the last
 * one stopped, or give a dense round to publish as it is: the record of each pair, and the outer columns copied by
 * value from the round's outer batch, fetched once per round. False when
 * no pair is left.
 */
static bool
fill_compact(TessHashJoinState *state)
{
	int			count = 0;

	/* The parent released the previous compact batch: its copies go. */
	MemoryContextReset(state->compact_context);
	/* LEFT: the rows without a match the last compact batch held back. */
	if (state->null_held)
	{
		state->null_held = false;
		(void) start_null_round(state);
		state->output_compact = false;
		return true;
	}

	while (count < JOIN_COMPACT_ROWS)
	{
		int			nrows;
		int			nwords;

		if (!state->round_open)
		{
			/* The pairs copied so far need the table they came from. */
			if (state->spill != NULL)
				state->spill->holding = count > 0;
			if (!next_round(state))
				break;
			/*
			 * LEFT: the rows without a match go out over the outer batch,
			 * after the pairs copied so far.
			 */
			if (state->null_round)
			{
				if (count == 0)
				{
					state->output_compact = false;
					return true;
				}
				/* The pairs go first, their inner columns gathered. */
				state->null_held = true;
				state->null_round = false;
				break;
			}
			nrows = state->outer_batch->rows.nrows;
			/*
			 * A dense round, met with nothing copied yet, goes out as it is:
			 * copying it would only cost, a by-reference value most.
			 */
			if (count == 0 &&
				tess_row_mask_count(&(TessRowMask) {nrows, state->round_bits}) >=
				JOIN_DENSE_ROUND)
			{
				state->output_compact = false;
				return true;
			}
			memcpy(state->taken_bits, state->round_bits,
				   sizeof(uint64) * tess_row_mask_word_count(nrows));
			for (int index = 0; index < state->nouter; index++)
			{
				int			column = state->outer_columns[index];

				/*
				 * The round's rows only: the next rounds are among them,
				 * and a lazy child need not read the rows without a pair.
				 */
				child_column(state->outer_batch, state->child_columns[column],
							 &(TessRowMask) {nrows, state->round_bits},
							 TESS_COLUMN_FOR_PROJECTION,
							 &state->round_columns[index]);
			}
			state->round_open = true;
		}
		nrows = state->outer_batch->rows.nrows;
		nwords = tess_row_mask_word_count(nrows);
		for (int index = 0; index < nwords && count < JOIN_COMPACT_ROWS; index++)
		{
			uint64		bits = state->taken_bits[index];

			while (bits != 0 && count < JOIN_COMPACT_ROWS)
			{
				int			row = index * 64 + pg_rightmost_one_pos64(bits);

				bits &= bits - 1;
				state->compact_offsets[count] = state->offsets[row];
				for (int column = 0; column < state->nouter; column++)
				{
					int			scan = state->outer_columns[column];

					Datum		value = state->round_columns[column].values[row];
					bool		isnull = state->round_columns[column].isnull[row];

					/*
					 * A by-reference value points into the outer batch,
					 * which goes before the compact batch does: a copy.
					 */
					if (!isnull && !state->typbyvals[scan])
					{
						MemoryContext oldcontext =
							MemoryContextSwitchTo(state->compact_context);

						value = datumCopy(value, false, state->typlens[scan]);
						MemoryContextSwitchTo(oldcontext);
					}
					state->compact_values[scan][count] = value;
					state->compact_isnull[scan][count] = isnull;
				}
				count++;
			}
			state->taken_bits[index] = bits;
		}
		/* A round copied whole: the next call of next_round advances. */
		for (int index = 0; index < nwords; index++)
			if (state->taken_bits[index] != 0)
				goto more;
		state->round_open = false;
more:
		;
	}
	if (count == 0)
		return false;
	state->output_compact = true;
	state->compact_bits[0] = count == 64 ? ~UINT64CONST(0) :
		(UINT64CONST(1) << count) - 1;
	state->counters[JOIN_COMPACT_BATCHES]++;
	state->batch.rows.nrows = JOIN_COMPACT_ROWS;
	state->batch.rows.bits = state->compact_bits;
	state->current_offsets = state->compact_offsets;
	state->current_bits = state->compact_bits;
	state->nulls_gathered = false;
	memset(state->gathered, 0, sizeof(bool) * Max(state->npayload, 1));
	return true;
}

/*
 * SEMI and ANTI: each outer batch once, its rows with a match (SEMI) or
 * without one (ANTI), a row with a NULL key never having one. A pair
 * counts when it passes the join clauses; rounds walk each row's records
 * until it has one, the rows that do leaving the next rounds. ANTI's
 * filters then apply to the rows it returns. False at the end.
 */
static bool
next_matches(TessHashJoinState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;

	for (;;)
	{
		TessBatch  *batch;
		int			nrows;
		int			nwords;
		uint64		any = 0;

		if (state->outer_batch != NULL)
		{
			outer_finish(state, state->outer_batch);
			state->outer_batch = NULL;
		}
		batch = outer_next(state);
		if (batch == NULL)
			return false;
		nrows = batch->rows.nrows;
		nwords = tess_row_mask_word_count(nrows);
		reserve_rows(state, nrows);
		memset(state->matched_bits, 0, sizeof(uint64) * nwords);
		state->outer_batch = batch;
		/* A table that spills has inner rows on disk even with none resident. */
		if ((state->build_rows > 0 ||
			 (state->spill != NULL && !state->spill->joining)) &&
			probe_batch(state, batch))
		{
			if (state->qual == NULL)
				memcpy(state->matched_bits, state->round_bits, sizeof(uint64) * nwords);
			else
				for (;;)
				{
					TessRowMask round = {nrows, state->round_bits};
					TessRowMask rest = {nrows, state->next_bits};
					uint64		left = 0;

					start_round(state);
					ResetExprContext(econtext);
					(void) tess_qual_apply(state->qual, &state->batch, econtext,
										   tess_row_mask_count(&round));
					for (int word = 0; word < nwords; word++)
						state->matched_bits[word] |= state->published_bits[word];
					if (state->inner_unique || state->duplicates == 0)
						break;
					for (int word = 0; word < nwords; word++)
					{
						state->next_bits[word] = state->round_bits[word] &
							~state->matched_bits[word];
						left |= state->next_bits[word];
					}
					if (left == 0)
						break;
					check(state, next_record(state, &rest, &round));
					if (tess_row_mask_count(&round) == 0)
						break;
				}
		}
		/* The rows returned: SEMI the matched ones, ANTI the others. */
		for (int word = 0; word < nwords; word++)
		{
			state->published_bits[word] = state->jointype == JOIN_SEMI ?
				state->matched_bits[word] :
				state->active_bits[word] & ~state->matched_bits[word];
			any |= state->published_bits[word];
		}
		if (any == 0)
			continue;
		state->batch.rows.nrows = nrows;
		state->batch.rows.bits = state->published_bits;
		state->current_offsets = state->offsets;
		state->current_bits = state->published_bits;
		state->nulls_gathered = false;
		memset(state->gathered, 0, sizeof(bool) * Max(state->npayload, 1));
		if (state->filter != NULL)
		{
			ResetExprContext(econtext);
			if (tess_qual_apply(state->filter, &state->batch, econtext,
								tess_row_mask_count(&state->batch.rows)) == 0)
				continue;
		}
		return true;
	}
}

/*
 * The next batch of pairs, a round or a compact batch, with the residual
 * join clauses applied: a batch they leave empty is skipped. The clauses
 * narrow the published selection only; a round's own rows stay whole for
 * the next round. False at the end.
 */
static bool
next_output(TessHashJoinState *state)
{
	ExprContext *econtext = state->css.ss.ps.ps_ExprContext;

	if (state->jointype == JOIN_SEMI || state->jointype == JOIN_ANTI)
		return next_matches(state);
	for (;;)
	{
		if (state->compact ? !fill_compact(state) : !next_round(state))
			return false;
		/* The join clauses decide the pairs; the rows without one have none. */
		if (state->qual != NULL && !state->null_round)
		{
			ResetExprContext(econtext);
			if (tess_qual_apply(state->qual, &state->batch, econtext,
								tess_row_mask_count(&state->batch.rows)) == 0)
				continue;
			/* LEFT: the pairs that passed; no compact batch with join clauses. */
			if (state->jointype == JOIN_LEFT)
				for (int word = 0; word < tess_row_mask_word_count(state->batch.rows.nrows); word++)
					state->matched_bits[word] |= state->published_bits[word];
		}
		/* An outer join's filters over every row it returns. */
		if (state->filter != NULL)
		{
			ResetExprContext(econtext);
			if (tess_qual_apply(state->filter, &state->batch, econtext,
								tess_row_mask_count(&state->batch.rows)) == 0)
				continue;
		}
		return true;
	}
}

/* Publish each round to a batch-aware parent, which finishes it there. */
static TupleTableSlot *
exec_batches(TessHashJoinState *state)
{
	/* Releasing a projection's wrapper forgets the pairs, which stay the node's. */
	tess_output_release(state->output);
	if (!next_output(state))
	{
		state->done = true;
		return NULL;
	}
	state->published = state->projection == NULL ? &state->batch :
		tess_projection_wrap(state->projection, &state->batch);
	return tess_output_publish(state->output, state->published);
}

/* Row mode: the column of every slot attribute, for the whole round. */
static void
fetch_columns(TessHashJoinState *state)
{
	int			natts = state->css.ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;

	for (int attribute = 0; attribute < natts; attribute++)
	{
		TessDatumColumn *column = &state->columns[attribute];

		*column = (TessDatumColumn) TESS_STRUCT_INITIALIZER(TessDatumColumn);
		state->published->ops->get_datum_column(state->published,
												tess_layout_column(&state->layout, attribute),
												&state->published->rows,
												TESS_COLUMN_FOR_PROJECTION, column);
		if (column->values == NULL || column->isnull == NULL ||
			column->nrows != state->batch.rows.nrows)
			elog(ERROR, "Tessera batch returned an invalid column");
	}
}

/* Serve the rows of each round from the node's own slot. */
static TupleTableSlot *
exec_rows(TessHashJoinState *state)
{
	TupleTableSlot *slot = state->css.ss.ps.ps_ResultTupleSlot;
	int			natts = slot->tts_tupleDescriptor->natts;
	int			row;

	for (;;)
	{
		if (!state->serving)
		{
			/* The previous round's wrapper, and its computed values, go now. */
			if (state->published != NULL && state->published != &state->batch)
				state->published->ops->release(state->published);
			state->published = NULL;
			if (!next_output(state))
			{
				state->done = true;
				return NULL;
			}
			state->published = state->projection == NULL ? &state->batch :
				tess_projection_wrap(state->projection, &state->batch);
			fetch_columns(state);
			state->next_row = tess_row_mask_next(&state->batch.rows, -1);
			state->serving = true;
		}
		if (state->next_row >= 0)
			break;
		state->serving = false;
	}
	row = state->next_row;
	state->next_row = tess_row_mask_next(&state->batch.rows, row);
	ExecClearTuple(slot);
	for (int attribute = 0; attribute < natts; attribute++)
	{
		slot->tts_values[attribute] = state->columns[attribute].values[row];
		slot->tts_isnull[attribute] = state->columns[attribute].isnull[row];
	}
	return ExecStoreVirtualTuple(slot);
}

/*
 * The children's requests, from the parent's: the outer columns asked for
 * come from the outer batches, the inner ones are kept in the payload, and
 * each child also gives its key. A row-wise parent reads every column of
 * the result slot.
 */
static void
send_requests(TessHashJoinState *state)
{
	TessRequest outer_request = TESS_STRUCT_INITIALIZER(TessRequest);
	TessRequest inner_request = TESS_STRUCT_INITIALIZER(TessRequest);
	const TessRequest *request = tess_output_request(state->output);
	Bitmapset  *needed = bms_union(request->filter_columns,
								   request->projection_columns);
	Bitmapset  *outer_columns = NULL;
	Bitmapset  *inner_columns = NULL;
	Bitmapset  *outer_key = NULL;
	Bitmapset  *inner_key = NULL;
	int			column = -1;

	for (int key = 0; key < state->nkeys; key++)
	{
		outer_key = bms_add_member(outer_key, state->outer_keys[key]);
		inner_key = bms_add_member(inner_key, state->inner_keys[key]);
	}
	/* The residual clauses read their columns of the pairs too. */
	if (state->qual != NULL)
		needed = bms_add_members(needed, tess_qual_columns(state->qual));
	if (request->output_mode == TESS_OUTPUT_ROWS)
	{
		int			natts = state->css.ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts;

		for (int attribute = 0; attribute < natts; attribute++)
			needed = bms_add_member(needed,
									tess_layout_column(&state->layout, attribute));
	}
	/* A computed column is the node's: the pairs give the columns it reads. */
	foreach_node(TargetEntry, entry, state->computed)
	{
		int			target = state->ncolumns + foreach_current_index(entry);

		if (!bms_is_member(target, needed))
			continue;
		needed = bms_del_member(needed, target);
		foreach_node(Var, var, pull_var_clause((Node *) entry->expr, 0))
			needed = bms_add_member(needed, var->varattno - 1);
	}
	state->npayload = 0;
	while ((column = bms_next_member(needed, column)) >= 0)
	{
		if (column >= state->ncolumns)
			elog(ERROR, "TessHashJoin has no column %d", column);
		if (state->sides[column] == JOIN_SIDE_OUTER)
		{
			outer_columns = bms_add_member(outer_columns,
										   state->child_columns[column]);
			state->outer_columns[state->nouter++] = column;
			continue;
		}
		if (state->npayload == JOIN_MAX_PAYLOAD)
			elog(ERROR, "TessHashJoin keeps at most %d inner columns",
				 JOIN_MAX_PAYLOAD);
		inner_columns = bms_add_member(inner_columns, state->child_columns[column]);
		state->payload_columns[state->npayload] = column;
		state->payload_words[column] = ++state->npayload;
	}
	/* The keys before any other column, then the rows that survive them. */
	outer_request.filter_columns = outer_key;
	outer_request.projection_columns = outer_columns;
	outer_request.output_mode = TESS_OUTPUT_BATCH;
	outer_request.max_batch_rows = request->max_batch_rows;
	tess_input_set_request(state->outer_input, &outer_request);
	inner_request.filter_columns = inner_key;
	inner_request.projection_columns = inner_columns;
	inner_request.output_mode = TESS_OUTPUT_BATCH;
	tess_input_set_request(state->inner_input, &inner_request);
	state->inner_values = palloc0_array(Datum *, Max(state->npayload, 1));
	state->inner_isnull = palloc0_array(bool *, Max(state->npayload, 1));
	state->gathered = palloc0_array(bool, Max(state->npayload, 1));
	state->request = request;
}

/* The clauses that run in batches and the others, each in evaluation order. */
static void
split_clauses(List *clauses, List *flags, List **batch, List **rows)
{
	ListCell   *clause;
	ListCell   *flag;

	*batch = NIL;
	*rows = NIL;
	forboth(clause, clauses, flag, flags)
	{
		if (lfirst_int(flag) != 0)
			*batch = lappend(*batch, lfirst(clause));
		else
			*rows = lappend(*rows, lfirst(clause));
	}
}

/* The plan's own data, written by the planner (join_planner.c). */
static void
read_node_data(TessHashJoinState *state, const List *data)
{
	TessPlanReader *reader = tess_plan_reader_create(data, TESS_HASH_JOIN_DATA,
													 TESS_HASH_JOIN_DATA_VERSION);
	List	   *sides = tess_plan_read_int_list(reader, "sides");
	List	   *columns = tess_plan_read_int_list(reader, "child_columns");
	List	   *outer_keys = tess_plan_read_int_list(reader, "outer_keys");
	List	   *inner_keys = tess_plan_read_int_list(reader, "inner_keys");
	List	   *outer_kinds = tess_plan_read_int_list(reader, "outer_kinds");
	List	   *inner_kinds = tess_plan_read_int_list(reader, "inner_kinds");
	ListCell   *side;
	ListCell   *column;
	int			index = 0;

	state->residual_batch = tess_plan_read_int_list(reader, "residual_batch");
	state->filter_batch = tess_plan_read_int_list(reader, "filter_batch");
	state->jointype = (JoinType) tess_plan_read_int(reader, "jointype");
	state->inner_unique = tess_plan_read_int(reader, "inner_unique") != 0;
	state->inner_rows = tess_plan_read_int(reader, "inner_rows");
	state->shared_mode = tess_plan_read_int(reader, "shared") != 0;
	tess_plan_reader_finish(reader);
	state->nkeys = list_length(outer_keys);
	if (list_length(sides) != state->ncolumns ||
		list_length(columns) != state->ncolumns ||
		state->nkeys < 1 || state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(inner_keys) != state->nkeys ||
		list_length(outer_kinds) != state->nkeys ||
		list_length(inner_kinds) != state->nkeys ||
		(state->jointype != JOIN_INNER && state->jointype != JOIN_SEMI &&
		 state->jointype != JOIN_ANTI && state->jointype != JOIN_LEFT) ||
		(state->filter_batch != NIL &&
		 state->jointype != JOIN_LEFT && state->jointype != JOIN_ANTI))
		elog(ERROR, "TessHashJoin received foreign plan data");
	for (int key = 0; key < state->nkeys; key++)
	{
		state->outer_keys[key] = list_nth_int(outer_keys, key);
		state->inner_keys[key] = list_nth_int(inner_keys, key);
		state->outer_kinds[key] = list_nth_int(outer_kinds, key);
		state->inner_kinds[key] = list_nth_int(inner_kinds, key);
		if ((state->outer_kinds[key] != TESS_TABLE_KEY_INT4 &&
			 state->outer_kinds[key] != TESS_TABLE_KEY_INT8) ||
			(state->inner_kinds[key] != TESS_TABLE_KEY_INT4 &&
			 state->inner_kinds[key] != TESS_TABLE_KEY_INT8))
			elog(ERROR, "TessHashJoin received foreign plan data");
	}
	state->sides = palloc_array(int, state->ncolumns);
	state->child_columns = palloc_array(int, state->ncolumns);
	forboth(side, sides, column, columns)
	{
		state->sides[index] = lfirst_int(side);
		state->child_columns[index] = lfirst_int(column);
		if (state->sides[index] != JOIN_SIDE_OUTER &&
			state->sides[index] != JOIN_SIDE_INNER)
			elog(ERROR, "TessHashJoin received foreign plan data");
		index++;
	}
}

static void
join_begin(CustomScanState *css, EState *estate, int eflags)
{
	TessHashJoinState *state = (TessHashJoinState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	TessPlanInfo info = TESS_STRUCT_INITIALIZER(TessPlanInfo);
	int			index = 0;

	/* The planner puts Material above a batch subtree for these. */
	if (eflags & (EXEC_FLAG_BACKWARD | EXEC_FLAG_MARK))
		elog(ERROR, "TessHashJoin supports neither backward scan nor mark/restore");
	tess_plan_get_info(cscan, &info);
	if (info.node != &tess_hash_join_node || info.nchildren != 2 ||
		info.child_names[0] == NULL || info.child_names[1] == NULL ||
		cscan->custom_scan_tlist == NIL)
		elog(ERROR, "TessHashJoin received a foreign plan");
	state->kernels = tess_runtime_kernels();
	if (state->kernels == NULL)
		ereport(ERROR,
				(errcode(ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE),
				 errmsg("TessHashJoin needs the Tessera kernels"),
				 errhint("Load tessera_kernels, or preload it with the other Tessera modules.")));
	state->ncolumns = list_length(cscan->custom_scan_tlist);
	read_node_data(state, (List *) info.node_data);
	state->typlens = palloc_array(int16, state->ncolumns);
	state->typbyvals = palloc_array(bool, state->ncolumns);
	foreach_ptr(TargetEntry, entry, cscan->custom_scan_tlist)
	{
		get_typlenbyval(exprType((Node *) entry->expr), &state->typlens[index],
						&state->typbyvals[index]);
		index++;
	}
	state->payload_words = palloc0_array(int, state->ncolumns);
	state->outer_columns = palloc0_array(int, state->ncolumns);
	state->compact_values = palloc0_array(Datum *, state->ncolumns);
	state->compact_isnull = palloc0_array(bool *, state->ncolumns);
	state->round_columns = palloc0_array(TessDatumColumn, Max(state->ncolumns, 1));
	state->payload_columns = palloc0_array(int, JOIN_MAX_PAYLOAD);

	state->outer = ExecInitNode(linitial(cscan->custom_plans), estate, eflags);
	state->inner = ExecInitNode(lsecond(cscan->custom_plans), estate, eflags);
	css->custom_ps = list_make2(state->outer, state->inner);
	state->outer_input = tess_input_create(estate->es_query_cxt, state->outer);
	state->inner_input = tess_input_create(estate->es_query_cxt, state->inner);
	state->layout = info.layout;
	state->output = tess_output_create(estate->es_query_cxt, &css->ss.ps,
									   css->ss.ps.ps_ResultTupleSlot,
									   &info.layout);
	state->table_context = AllocSetContextCreate(estate->es_query_cxt,
												 "TessHashJoin table",
												 ALLOCSET_DEFAULT_SIZES);
	state->values_context = AllocSetContextCreate(estate->es_query_cxt,
												  "TessHashJoin values",
												  ALLOCSET_DEFAULT_SIZES);
	reset_values(state);
	/* The residual clauses: those the compiler took in batches, then by rows. */
	state->scan_layout = (TessLayout) TESS_STRUCT_INITIALIZER(TessLayout);
	state->scan_layout.ncolumns = state->ncolumns;
	state->scan_layout.ntargets = state->ncolumns;
	/* custom_exprs: the key clauses, the residual ones, an outer join's filters. */
	if (list_length(cscan->custom_exprs) !=
		state->nkeys + list_length(state->residual_batch) +
		list_length(state->filter_batch))
		elog(ERROR, "TessHashJoin received a foreign plan");
	if (state->filter_batch != NIL)
	{
		TessQualConfig filter = TESS_STRUCT_INITIALIZER(TessQualConfig);
		List	   *filters = list_copy_tail(cscan->custom_exprs,
											 state->nkeys +
											 list_length(state->residual_batch));

		filter.parent_context = estate->es_query_cxt;
		filter.parent = &css->ss.ps;
		split_clauses(filters, state->filter_batch, &filter.batch_clauses,
					  &filter.row_clauses);
		filter.order = state->filter_batch;
		filter.scan_slot = css->ss.ss_ScanTupleSlot;
		filter.scan_tuple = &state->scan_layout;
		state->filter = tess_qual_create(&filter);
	}
	if (state->residual_batch != NIL)
	{
		TessQualConfig qual = TESS_STRUCT_INITIALIZER(TessQualConfig);

		List	   *residual = list_copy_head(list_copy_tail(cscan->custom_exprs,
															  state->nkeys),
											  list_length(state->residual_batch));

		qual.parent_context = estate->es_query_cxt;
		qual.parent = &css->ss.ps;
		split_clauses(residual, state->residual_batch, &qual.batch_clauses,
					  &qual.row_clauses);
		qual.order = state->residual_batch;
		qual.scan_slot = css->ss.ss_ScanTupleSlot;
		qual.scan_tuple = &state->scan_layout;
		state->qual = tess_qual_create(&qual);
	}
	if (info.computed != NIL)
	{
		TessProjectionConfig projection = TESS_STRUCT_INITIALIZER(TessProjectionConfig);

		/* Computed columns follow the scan tuple's, as for TessFilter. */
		projection.parent_context = estate->es_query_cxt;
		projection.parent = &css->ss.ps;
		projection.econtext = css->ss.ps.ps_ExprContext;
		projection.scan_slot = css->ss.ss_ScanTupleSlot;
		projection.scan_tuple = &state->scan_layout;
		projection.base_columns = state->ncolumns;
		projection.computed = info.computed;
		state->projection = tess_projection_create(&projection);
		state->computed = info.computed;
	}
	state->status = (TessStatus) TESS_STRUCT_INITIALIZER(TessStatus);
	state->batch = (TessBatch) {
		TESS_ABI_INITIALIZER(TESS_BATCH_ABI_VERSION, TessBatch),
	};
	state->batch.table_oid = InvalidOid;
	state->batch.ops = &join_batch_ops;
	state->batch.private_data = state;
	state->columns = palloc0_array(TessDatumColumn,
								   Max(css->ss.ps.ps_ResultTupleSlot->tts_tupleDescriptor->natts, 1));
	state->next_row = -1;
}

/*
 * Compact mode for a batch-aware parent over a table with duplicate keys;
 * a by-reference outer value is copied, since it must outlive its outer
 * batch.
 */
static void
decide_compact(TessHashJoinState *state)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;

	state->compact_decided = true;
	state->compact = false;
	/*
	 * SEMI and ANTI return outer rows, not pairs; LEFT with join clauses
	 * must see each round whole to know the rows without a match.
	 */
	if (state->request->output_mode != TESS_OUTPUT_BATCH ||
		state->inner_unique || state->duplicates == 0 ||
		state->jointype == JOIN_SEMI || state->jointype == JOIN_ANTI ||
		(state->jointype == JOIN_LEFT && state->qual != NULL))
		return;
	for (int index = 0; index < state->nouter; index++)
	{
		int			column = state->outer_columns[index];

		if (state->compact_values[column] != NULL)
			continue;
		state->compact_values[column] =
			MemoryContextAllocZero(context, sizeof(Datum) * JOIN_COMPACT_ROWS);
		state->compact_isnull[column] =
			MemoryContextAllocZero(context, sizeof(bool) * JOIN_COMPACT_ROWS);
	}
	/* The copies of by-reference values, for one compact batch at a time. */
	if (state->compact_context == NULL)
		state->compact_context = AllocSetContextCreate(context,
													   "TessHashJoin compact values",
													   ALLOCSET_DEFAULT_SIZES);
	state->compact = true;
}

static TupleTableSlot *
join_exec(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->done)
		return NULL;
	if (state->request == NULL)
		send_requests(state);
	if (!state->built)
	{
		if (state->shared != NULL)
			build_shared(state);
		else
			build_table(state);
		if (state->done)
			return NULL;
	}
	/*
	 * Nothing to match: the outer child is never read, as in the core,
	 * unless its rows go out without a match (LEFT, ANTI).
	 */
	if ((state->spill != NULL ? state->spill->total_rows : state->build_rows) == 0 &&
		(state->jointype == JOIN_INNER || state->jointype == JOIN_SEMI))
	{
		state->done = true;
		return NULL;
	}
	if (!state->compact_decided)
		decide_compact(state);
	return state->request->output_mode == TESS_OUTPUT_BATCH ?
		exec_batches(state) : exec_rows(state);
}

static void
join_end(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	tess_output_end(state->output);
	ExecEndNode(state->outer);
	ExecEndNode(state->inner);
	spill_free(state);
	MemoryContextDelete(state->values_context);
	MemoryContextDelete(state->table_context);
}

/*
 * The core passes changed parameters to the children; the table is built
 * again only when the inner child depends on one, as the core's hash join
 * decides, and is otherwise probed by the rescanned outer child again.
 */
static void
join_rescan(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	tess_output_clear(state->output);
	ExecClearTuple(css->ss.ps.ps_ResultTupleSlot);
	if (state->projection != NULL)
		tess_projection_reset(state->projection);
	state->published = NULL;
	/* The rescan forgets the outer batch with the child's other state. */
	state->outer_batch = NULL;
	state->round_open = false;
	state->null_round = false;
	state->null_held = false;
	state->serving = false;
	state->next_row = -1;
	state->done = false;
	if (css->ss.ps.chgParam != NULL)
	{
		UpdateChangedParamSet(state->outer, css->ss.ps.chgParam);
		UpdateChangedParamSet(state->inner, css->ss.ps.chgParam);
	}
	/*
	 * A shared table goes with its build, which starts anew with the
	 * rescan's workers; the leader leaves the one it took part in.
	 */
	if (state->shared != NULL)
		leave_shared(state);
	/*
	 * A table that spilled is no longer whole: the inner child is read
	 * again, rescanned here when no parameter of it changed.
	 */
	if (state->spill != NULL)
	{
		spill_free(state);
		if (state->inner->chgParam == NULL)
			ExecReScan(state->inner);
		tess_input_rescan(state->inner_input);
		state->built = false;
	}
	else if (state->inner->chgParam != NULL || state->shared != NULL)
	{
		/* The inner child rescans at its next execution. */
		tess_input_rescan(state->inner_input);
		state->built = false;
	}
	/* Without changed parameters, the executor would not rescan it. */
	if (state->outer->chgParam == NULL)
		ExecReScan(state->outer);
	tess_input_rescan(state->outer_input);
}

/* This participant's counters, the memory ones as of now. */
static void
join_counters(TessHashJoinState *state, uint64 *values)
{
	Size		limit = get_hash_memory_limit();

	memcpy(values, state->counters, sizeof(state->counters));
	if (state->qual != NULL)
	{
		const TessQualStats *removed = tess_qual_stats(state->qual);

		values[JOIN_FILTER_REMOVED] = removed->batch_removed + removed->row_removed;
	}
	if (state->filter != NULL)
	{
		const TessQualStats *removed = tess_qual_stats(state->filter);

		values[JOIN_OUTPUT_REMOVED] = removed->batch_removed + removed->row_removed;
	}
	values[JOIN_MEMORY] = state->peak_memory;
	values[JOIN_OVERRUN] = state->peak_memory > limit ?
		state->peak_memory - limit : 0;
}

/*
 * The join clause; with ANALYZE, the table and the rows through it, the
 * totals of every participant in a parallel plan, each of which builds a
 * table of its own. The memory is the most the tables and the copies of
 * inner values took, and Overrun what of it exceeded hash_mem: the node
 * keeps the whole inner side in memory rather than spilling it.
 */
static void
join_explain(CustomScanState *css, List *ancestors, ExplainState *es)
{
	TessHashJoinState *state = (TessHashJoinState *) css;
	CustomScan *cscan = castNode(CustomScan, css->ss.ps.plan);
	bool		useprefix = es->rtable_size > 1 || es->verbose;
	List	   *context;
	const uint64 *totals = NULL;
	uint64		own[JOIN_NCOUNTERS];

	context = set_deparse_context_plan(es->deparse_cxt, css->ss.ps.plan,
									   ancestors);
	if (state->jointype != JOIN_INNER)
		ExplainPropertyText("Join Type",
							state->jointype == JOIN_SEMI ? "Semi" :
							state->jointype == JOIN_ANTI ? "Anti" : "Left", es);
	ExplainPropertyText("Hash Cond",
						deparse_expression((Node *) make_ands_explicit(list_copy_head(cscan->custom_exprs,
																					  state->nkeys)),
										   context, useprefix, false), es);
	if (state->shared_mode)
		ExplainPropertyBool("Shared Table", true, es);
	for (int part = 0; part < 2; part++)
	{
		/*
		 * The residual join clauses, then an outer join's filters: those
		 * the compiler took in batches, and the others.
		 */
		List	   *flags = part == 0 ? state->residual_batch : state->filter_batch;
		int			first = state->nkeys +
			(part == 0 ? 0 : list_length(state->residual_batch));
		List	   *clauses = list_copy_head(list_copy_tail(cscan->custom_exprs, first),
											 list_length(flags));
		List	   *batch;
		List	   *rows;

		split_clauses(clauses, flags, &batch, &rows);
		if (batch != NIL)
			ExplainPropertyText(part == 0 ? "Batch Join Filter" : "Batch Filter",
								deparse_expression((Node *) make_ands_explicit(batch),
												   context, useprefix, false), es);
		if (rows != NIL)
			ExplainPropertyText(part == 0 ? "Join Filter" : "Filter",
								deparse_expression((Node *) make_ands_explicit(rows),
												   context, useprefix, false), es);
	}
	if (!es->analyze)
		return;
	if (state->stats != NULL)
		totals = tess_shared_stats_totals(state->stats);
	if (totals == NULL)
	{
		join_counters(state, own);
		totals = own;
	}
	ExplainPropertyInteger("Buckets", NULL,
						   totals[JOIN_BUILDS] > 0 ?
						   totals[JOIN_BUCKETS] / totals[JOIN_BUILDS] : 0, es);
	ExplainPropertyInteger("Memory Usage", "kB",
						   (totals[JOIN_MEMORY] + 1023) / 1024, es);
	if (totals[JOIN_OVERRUN] > 0)
		ExplainPropertyInteger("Overrun", "kB",
							   (totals[JOIN_OVERRUN] + 1023) / 1024, es);
	ExplainPropertyInteger("Builds", NULL, totals[JOIN_BUILDS], es);
	ExplainPropertyInteger("Build Rows", NULL, totals[JOIN_BUILD_ROWS], es);
	ExplainPropertyInteger("Chunks", NULL, totals[JOIN_CHUNKS], es);
	if (totals[JOIN_BATCHES] > 0)
	{
		ExplainPropertyInteger("Batches", NULL, totals[JOIN_BATCHES], es);
		ExplainPropertyInteger("Resident Partitions", NULL, totals[JOIN_RESIDENT], es);
		ExplainPropertyInteger("Spilled Chunks", NULL, totals[JOIN_SPILLED], es);
		ExplainPropertyInteger("Disk Usage", "kB", (totals[JOIN_DISK] + 1023) / 1024, es);
		ExplainPropertyInteger("Tail Chunks Kept", NULL, totals[JOIN_TAILS], es);
		if (totals[JOIN_PASSES] > 0)
			ExplainPropertyInteger("Extra Passes", NULL, totals[JOIN_PASSES], es);
	}
	ExplainPropertyInteger("Probe Rows", NULL, totals[JOIN_PROBE_ROWS], es);
	ExplainPropertyInteger("Matches", NULL, totals[JOIN_MATCHES], es);
	if (state->qual != NULL)
		ExplainPropertyInteger("Rows Removed by Join Filter", NULL,
							   totals[JOIN_FILTER_REMOVED], es);
	if (state->filter != NULL)
		ExplainPropertyInteger("Rows Removed by Filter", NULL,
							   totals[JOIN_OUTPUT_REMOVED], es);
	if (totals[JOIN_COMPACT_BATCHES] > 0)
		ExplainPropertyInteger("Compact Batches", NULL,
							   totals[JOIN_COMPACT_BATCHES], es);
	if (totals[JOIN_BLOOM_FILTERS] > 0)
	{
		ExplainPropertyInteger("Bloom Filters", NULL,
							   totals[JOIN_BLOOM_FILTERS], es);
		/* A child that took the filter shows the rows it removed instead. */
		if (totals[JOIN_BLOOM_BELOW] == 0 || totals[JOIN_BLOOM_REMOVED] > 0)
			ExplainPropertyInteger("Rows Removed by Bloom Filter", NULL,
								   totals[JOIN_BLOOM_REMOVED], es);
	}
	/* The outer child checked its rows: it shows the rows removed. */
	if (totals[JOIN_BLOOM_BELOW] > 0)
		ExplainPropertyBool("Bloom Filter Below", true, es);
}

/*
 * A parallel plan: the outer child divides the rows, and every
 * participant builds the whole inner side into a table of its own, as the
 * core's hash join without a shared table does. The node shares only its
 * counters, in the rows of its chunk.
 */
/* The bytes of the chunk a shared build takes before the counters. */
static Size
shared_size(TessHashJoinState *state)
{
	return state->shared_mode ? MAXALIGN(sizeof(JoinShared)) : 0;
}

/* A shared build's state before any participant attaches. */
static void
init_shared(TessHashJoinState *state)
{
	BarrierInit(&state->shared->build, 0);
	state->shared->index = InvalidDsaPointer;
	state->shared->index_len = 0;
	state->shared->directory = InvalidDsaPointer;
	state->shared->nchunks = 0;
	state->shared->filter = InvalidDsaPointer;
	state->shared->filter_words = 0;
	SpinLockInit(&state->shared->lock);
	state->shared->chunks = InvalidDsaPointer;
	state->shared->values = InvalidDsaPointer;
	state->shared->next_value_chunk = 0;
	state->shared->value_directory = InvalidDsaPointer;
	state->shared->nvalue_chunks = 0;
	check(state, state->kernels->build_counters_init(state->shared->counters,
													 &state->status));
	memset(&state->participant, 0, sizeof(state->participant));
	state->participating = false;
}

static Size
join_estimate_dsm(CustomScanState *css, ParallelContext *pcxt)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	return add_size(shared_size(state),
					tess_shared_stats_estimate(JOIN_NCOUNTERS, pcxt->nworkers));
}

static void
join_initialize_dsm(CustomScanState *css, ParallelContext *pcxt,
					void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->shared_mode)
	{
		state->shared = coordinate;
		init_shared(state);
	}
	/* A Gather a limit above shut down sets up anew when rescanned. */
	if (state->stats != NULL)
		tess_shared_stats_end(state->stats);
	state->stats = tess_shared_stats_init(css->ss.ps.state->es_query_cxt,
										  (char *) coordinate + shared_size(state),
										  JOIN_NCOUNTERS, pcxt->nworkers, pcxt->seg);
}

/*
 * Before a rescan's workers start: the leader leaves a build it still
 * takes part in, and the table, which a participant that stopped early
 * may have left behind, is freed; the next execution builds anew.
 */
static void
join_reinitialize_dsm(CustomScanState *css, ParallelContext *pcxt,
					  void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->shared != NULL)
	{
		leave_shared(state);
		free_shared_table(state);
		init_shared(state);
		state->built = false;
	}
	tess_shared_stats_reset(state->stats);
}

static void
join_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->shared_mode)
		state->shared = coordinate;
	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt,
											(char *) coordinate + shared_size(state),
											ParallelWorkerNumber + 1);
}

static void
join_shutdown(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;
	uint64		values[JOIN_NCOUNTERS];

	if (state->shared != NULL)
		leave_shared(state);
	if (state->stats == NULL)
		return;
	join_counters(state, values);
	tess_shared_stats_store(state->stats, values);
}

static Node *
join_create_state(CustomScan *cscan)
{
	TessHashJoinState *state = (TessHashJoinState *)
		newNode(sizeof(TessHashJoinState), T_CustomScanState);

	state->css.methods = &join_exec_methods;
	return (Node *) state;
}

static const CustomExecMethods join_exec_methods = {
	.CustomName = "TessHashJoin",
	.BeginCustomScan = join_begin,
	.ExecCustomScan = join_exec,
	.EndCustomScan = join_end,
	.ReScanCustomScan = join_rescan,
	.ExplainCustomScan = join_explain,
	.EstimateDSMCustomScan = join_estimate_dsm,
	.InitializeDSMCustomScan = join_initialize_dsm,
	.ReInitializeDSMCustomScan = join_reinitialize_dsm,
	.InitializeWorkerCustomScan = join_initialize_worker,
	.ShutdownCustomScan = join_shutdown,
};

const CustomScanMethods tess_hash_join_scan_methods = {
	.CustomName = "TessHashJoin",
	.CreateCustomScanState = join_create_state,
};

const TessNode tess_hash_join_node = {
	TESS_ABI_INITIALIZER(TESS_NODE_ABI_VERSION, TessNode),
	.name = TESS_HASH_JOIN_NODE_NAME,
};

#include "postgres.h"

#include "access/parallel.h"
#include "commands/explain.h"
#include "commands/explain_format.h"
#include "executor/executor.h"
#include "miscadmin.h"
#include "port/pg_bitutils.h"
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
/*
 * Pruning of the outer side's partitions: the keys are listed, each
 * pruning by itself, while the inner side has at most this many rows with
 * a key; past them the lowest and the highest prune as a range.
 */
#define JOIN_PRUNE_VALUES 1024

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
	/* Partitions split again by the hash bits above their own. */
	JOIN_SPLITS,
	/*
	 * A shared table's partitions on disk: rounds that loaded one into
	 * shared memory for every participant, counted by the one that made
	 * its index, and those one participant joined alone.
	 */
	JOIN_ROUNDS,
	JOIN_ALONE,
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
	/* The participant that made it, whose files a table that spills writes it to. */
	int			owner;
} JoinChunk;

#define JOIN_CHUNK_HEADER MAXALIGN(sizeof(JoinChunk))

/*
 * A shared table's budget counts, besides its chunks, the index and the
 * filter SIZE makes for their records: a bucket of 4 bytes, twice the
 * records rounded up to a power of two (8 to 16 bytes a record), and 16
 * bits of filter rounded up the same way.
 */
#define JOIN_INDEX_PER_RECORD 16

/* The bytes a chunk of records of len bytes costs a shared table's budget. */
static inline int64
record_chunk_cost(Size len, Size record_size)
{
	return (int64) (JOIN_CHUNK_HEADER + len +
					JOIN_INDEX_PER_RECORD * (len / Max(record_size, 1)));
}

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
	 * Every chunk of records and every chunk of values, in lists of each
	 * participant's own, which only that participant adds to or takes
	 * from before SIZE: an array of two heads per participant, its chunks'
	 * and its value chunks'. The lock numbers the value chunks; the
	 * elected participant makes the value chunks' directory as it does
	 * the records'.
	 */
	slock_t		lock;
	dsa_pointer lists;
	uint32		next_value_chunk;
	dsa_pointer value_directory;
	int			nvalue_chunks;
	/*
	 * Spilling (docs/spill.md): the words every participant decides by
	 * (tess_table_spill_*), the filter of every inner row once the table
	 * is split, the rows of the partitions in memory, the most
	 * participants, whose files a partition is read from, the segment a
	 * worker attaches the files through, and the files.
	 */
	dsa_pointer spill_words;
	Size		spill_nwords;
	dsa_pointer spill_filter;
	Size		spill_filter_words;
	uint64		resident_rows;
	int			participants;
	dsm_handle	segment;
	SharedFileSet fileset;
	/*
	 * Per partition, as JOIN_SPILL_MAX_PARTITIONS pairs of atomics: the
	 * bytes of the inner side's blocks on disk and its blocks of records,
	 * every participant's, added at FLUSH; then the rounds over the
	 * partitions, which SIZE makes once the table split.
	 */
	dsa_pointer part_stats;
	dsa_pointer rounds;
	int			nrounds;
	/* RIGHT and FULL: the marks of the table's records (mark_words per chunk). */
	dsa_pointer marks;
	/*
	 * Pruning of the outer side's partitions: the inner rows with a key, the
	 * lowest and the highest, and the keys while at most JOIN_PRUNE_VALUES
	 * (-1 past them); every participant adds its own at the end of its
	 * build, under prune_lock, so that they are whole past the barrier.
	 */
	slock_t		prune_lock;
	uint64		prune_rows;
	int64		prune_min;
	int64		prune_max;
	int			prune_nvalues;
	int64		prune_values[JOIN_PRUNE_VALUES];
} JoinShared;

/*
 * A round over a partition of a shared table on disk small enough for one
 * participant's hash_mem (docs/spill.md, "A shared table"): the
 * participants that attach elect one, which makes its index in the query's
 * shared memory for its records; all load the inner files, taken one at
 * a time, each block into a block of shared memory, numbering the chunks
 * of records by a counter and linking them as they go; all probe with its
 * outer files, taken one at a time, and leave without waiting, the last
 * one freeing it. The phases are tess_round_step's.
 */
typedef struct JoinRound
{
	Barrier		barrier;
	/* Set by SIZE: whether the participants load it together, its records, chunks and value chunks. */
	bool		together;
	uint64		records;
	int			nchunks;
	int			nvalues;
	/* Set by the elected one: the index, and the directories of the chunks and of the value chunks. */
	dsa_pointer index;
	Size		index_len;
	dsa_pointer directory;
	dsa_pointer values;
	pg_atomic_uint32 next_chunk;
	/* RIGHT and FULL: the marks of its records, as a shared table's. */
	dsa_pointer marks;
} JoinRound;

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
	/* Bytes of the blocks written, and the blocks of records among them. */
	uint64		disk_bytes;
	uint64		blocks;
	bool		resident;
	/* A block of the partition went to disk. */
	bool		written;
	/* The partition's values outgrew a chunk: it is written after the batch. */
	bool		queued;
	/* The last batch that saw a row of it pending, to visit it once. */
	uint64		stamp;
} SpillPart;

/*
 * One side's rows by partition: the inner rows while building, as
 * records of the table's format, and the outer rows of the partitions on
 * disk while probing, as chunks of columns; a partition appends to chunk
 * current[partition], and chunk 0 is empty, with no room, for a
 * partition with no chunk yet. The by-reference values are in value
 * chunks numbered for the side, which a payload word refers to as a
 * table's does.
 */
typedef struct SpillSide
{
	MemoryContext context;
	/*
	 * The outer side keeps its rows in chunks of columns (tessera/spill.h),
	 * written and read back, never linked; the inner side in records.
	 */
	bool		columnar;
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
	/* The rows of each partition, which the appends count. */
	uint64	   *rows;
	TessSpill  *file;
	uint64		fingerprint;
	uint32		next_number;
	Size		bytes;
	/* Partitions whose values outgrew a chunk, written after the batch. */
	int		   *queue;
	int			nqueue;
	/*
	 * A shared table's inner side: its chunks and value chunks in the
	 * query's shared memory, each after a JoinChunk header, by pointer as
	 * well as by address; their bytes counted by partition in the words of
	 * the table's spilling, which say whether they pass the budget; the
	 * value chunks numbered for every participant under the table's lock.
	 * NULL area for a side in memory of its own.
	 */
	dsa_area   *area;
	struct JoinShared *shared;
	uint64	   *spill_words;
	Size		spill_nwords;
	const TessKernelOps *kernels;
	dsa_pointer *pointers;
	dsa_pointer *value_pointers;
	bool		over;
	int			owner;
	/*
	 * The files are in the table's set, where every participant reads
	 * them: a partition's are kept once joined, and deleted with the set.
	 */
	bool		shared_files;
} SpillSide;

/*
 * A partition's blocks as they were written, from the file of each
 * participant that wrote to it in turn: one file in a serial plan.
 */
typedef struct PartReader
{
	TessSpill  *file;
	int			partition;
	int			writers;
	int			next;
	TessSpillReader *reader;
	bool		open;
} PartReader;

typedef struct JoinSpill
{
	MemoryContext context;
	/*
	 * A partition still too large splits by the hash bits above its own
	 * into a level of its own, whose outer rows are the partition's: the
	 * level it came from and its depth; the inner rows it split, which a
	 * partition of it must have much fewer of to split again.
	 */
	struct JoinSpill *parent;
	uint32		level;
	uint64		input_rows;
	int			npartitions;
	uint32		shift;
	SpillSide	build;
	SpillSide	probe;
	/*
	 * A shared table's first level (docs/spill.md, "Shared tables"): the
	 * inner side in the query's shared memory, the files in the table's
	 * set, every participant's read for a partition, and the filter the
	 * table's; the outer rows the shared table answers, those of the
	 * partitions in memory and those without a pair, in a side of their
	 * own, and whether they are all read. The side the outer rows are read
	 * from.
	 */
	bool		shared;
	SpillSide	resident;
	bool		resident_done;
	SpillSide  *rows;
	/*
	 * A shared table's partitions on disk, visited from start on, spread
	 * over the participants: the visited count; a round's chunks as this
	 * participant links them (an empty chunk for the others'), and the
	 * bases of its value chunks.
	 */
	int			start;
	int			visited;
	void	  **round_bases;
	Size	   *round_lens;
	char	  **round_values;
	/* A Bloom filter of every inner row, checked before an outer row is written. */
	uint64	   *bloom;
	Size		bloom_words;
	uint64		total_rows;
	uint64		stamp;
	/* The outer child is done: the partitions on disk are joined in turn. */
	bool		joining;
	/*
	 * The resident partitions' index is made: its bytes are the table's
	 * now, no longer estimated per resident row.
	 */
	bool		indexed;
	/* The outer child returned its last batch. */
	bool		child_done;
	/* The participants whose files a partition is read from: 1 when serial. */
	int			writers;
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
	PartReader	build_reader;
	bool		pieces_done;
	bool		final_pass;
	void	   *carried;
	TessSpillHeader carried_header;
	uint64	   *matched_rows;
	Size		matched_words;
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
	 * of columns being read and the values before it, the next row of it,
	 * and the tail once the file is done.
	 */
	PartReader	reader;
	MemoryContext block_context;
	void	   *block;
	bool		tail_read;
	uint64		cursor;
	/* The batch of outer rows read back: each stored column's values. */
	TessBatch	batch;
	uint64		bits[1];
	/*
	 * Per stored word, the values of the window of a chunk read back: in
	 * the chunk's lane for a by-value word, else in values, the pointers
	 * of its references; and the NULL flags.
	 */
	Datum	  **window;
	Datum	  **values;
	bool	  **isnull;
	/* The outer child's column of each stored word, and the word of each child column plus one. */
	int		   *stored;
	int		   *word_of;
	int			nchild;
	/*
	 * Appending a batch's rows to a side: the inner child's column of each
	 * payload word of the inner side, the rows of a batch room is kept for,
	 * the rows pending before an append, their payloads, and the columns
	 * read.
	 */
	int		   *build_children;
	int			payload_rows;
	uint64	   *before_bits;
	uint8	  **payloads;
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
	/*
	 * A key a word does not hold: its type's 64-bit hash function, whose
	 * value is the table's key, a residual clause deciding the match; the
	 * hashes of a batch, in a context reset per batch. fn_oid is
	 * InvalidOid for a word key.
	 */
	FmgrInfo	hashers[TESS_TABLE_MAX_KEYS];
	Oid			collations[TESS_TABLE_MAX_KEYS];
	bool		hashed_keys;
	Datum	   *hash_values[TESS_TABLE_MAX_KEYS];
	bool	   *hash_isnull[TESS_TABLE_MAX_KEYS];
	MemoryContext hash_context;
	/* The key columns of the batch being inserted or probed. */
	TessDatumColumn key_columns[TESS_TABLE_MAX_KEYS];
	TessTableKey table_keys[TESS_TABLE_MAX_KEYS];
	/* Every outer row matches at most one inner row: no second round. */
	bool		inner_unique;
	/*
	 * INNER, SEMI, ANTI or LEFT: the kinds that keep the outer side, which
	 * the node probes with. RIGHT runs as INNER and FULL as LEFT, keeping
	 * the inner side too: preserve_inner, and the plan's kind for EXPLAIN.
	 */
	JoinType	jointype;
	JoinType	plan_jointype;
	bool		preserve_inner;
	/*
	 * RIGHT and FULL: per chunk of the table, a bit per record set once a
	 * pair of it passed the join clauses; after the outer side, the records
	 * without one go out with NULL outer columns (the tail), from the walk's
	 * chunk and byte on.
	 */
	uint64	  **marks;
	int			mark_slots;
	MemoryContext marks_context;
	/*
	 * A shared table's marks are in the query's shared memory, set with an
	 * atomic OR; the last participant to leave the table returns its tail
	 * and then frees it (a round's or the build's).
	 */
	bool		marks_shared;
	bool		round_departed;
	bool		round_free_owed;
	bool		table_free_owed;
	Size		record_size;
	bool		tail;
	/*
	 * The tail of the table in memory is done; a spilling join asks for
	 * one before each table it drops: the resident partitions', each
	 * piece's, each partition's.
	 */
	bool		table_tail_done;
	bool		tail_request;
	int			tail_chunk;
	Size		tail_byte;
	uint32		tail_refs[JOIN_COMPACT_ROWS];
	uint64		tail_bits[1];
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
	/*
	 * Pruning of the outer side's partitions (TessAppend, docs/nodes.md):
	 * the key's number, -1 for none, the planned descriptions and their
	 * parameters, whether the outer node took them, the keys of this build
	 * (this participant's share of a shared one) and whether the outer node
	 * has them since the last rescan.
	 */
	int			prune_key;
	PartitionPruneInfo *prune_values;
	PartitionPruneInfo *prune_range;
	int			prune_params[3];
	bool		prune_on;
	TessJoinKeys prune_keys;
	bool		prune_sent;
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
	/*
	 * A compact batch holds pairs of the table being probed: no other table
	 * is loaded until it goes out.
	 */
	bool		holding;

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
	/* A shared table's budget, every participant's hash_mem; 0 without one. */
	Size		shared_budget;
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
	/*
	 * A shared table that spills: the words of its spilling, mapped here;
	 * this participant's number in the files' names, the partitions sent
	 * to disk it saw, and whether the chunks passed the budget before the
	 * table was split.
	 */
	uint64	   *spill_words;
	int			spill_participant;
	uint64		spill_seen;
	bool		spill_over;
	/*
	 * The table probed is shared, the build's or a round's, whose chains are
	 * walked; the round this participant takes part in, -1 for none, and
	 * its place in the round's phases.
	 */
	bool		chain_table;
	int			round_partition;
	TessBuildParticipant round_participant;
} TessHashJoinState;

static const CustomExecMethods join_exec_methods;

static void reset_values(TessHashJoinState *state);
static void start_spill(TessHashJoinState *state);
static void insert_spill(TessHashJoinState *state, TessBatch *batch);
static void finish_spill_build(TessHashJoinState *state);
static void spill_free(TessHashJoinState *state);
static Size spill_memory(JoinSpill *spill, uint64 *resident);
static void decide_compact(TessHashJoinState *state);
static void shared_check(TessHashJoinState *state);
static void shared_flush(TessHashJoinState *state);
static void shared_outer(TessHashJoinState *state);
static void shared_probe_start(TessHashJoinState *state, uint64 records);
static TessBatch *shared_resident_next(TessHashJoinState *state);
static void shared_resident_end(TessHashJoinState *state);
static bool shared_has_outer(JoinSpill *spill, int partition);
static void make_rounds(TessHashJoinState *state);
static void free_rounds(TessHashJoinState *state);
static void round_leave(TessHashJoinState *state);
static bool round_depart(TessHashJoinState *state);
static bool leave_shared(TessHashJoinState *state, bool keep);
static bool round_next_outer(TessHashJoinState *state);
static bool shared_next_partition(TessHashJoinState *state);
static pg_atomic_uint64 *part_stats(TessHashJoinState *state, int partition);

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
 * Replace the values of a key a word does not hold by their 64-bit hashes
 * for the rows, NULL kept.
 */
static void
hash_key_column(TessHashJoinState *state, int key, const TessRowMask *rows,
				TessDatumColumn *column)
{
	FmgrInfo   *hasher = &state->hashers[key];
	Datum	   *values = state->hash_values[key];
	bool	   *isnull = state->hash_isnull[key];
	MemoryContext old = MemoryContextSwitchTo(state->hash_context);
	int			row = -1;

	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		isnull[row] = column->isnull[row];
		if (!isnull[row])
			values[row] = FunctionCall2Coll(hasher, state->collations[key],
											column->values[row], Int64GetDatum(0));
	}
	MemoryContextSwitchTo(old);
	column->values = values;
	column->isnull = isnull;
}

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
	/*
	 * A NULL key never matches; RIGHT and FULL still keep such an inner row
	 * as a record, which no probe finds and the tail returns.
	 */
	TessNullKeys nulls = state->preserve_inner && kinds == state->inner_kinds ?
		TESS_NULL_KEYS_GROUP : TESS_NULL_KEYS_REJECT;

	if (state->hashed_keys)
		MemoryContextReset(state->hash_context);
	for (int key = 0; key < state->nkeys; key++)
	{
		TessDatumColumn *keys = &state->key_columns[key];
		bool		int8 = kinds[key] == TESS_TABLE_KEY_INT8;
		const TessRowMask *rows = key == 0 ? &batch->rows : valid;

		child_column(batch, columns[key], rows, TESS_COLUMN_FOR_FILTER, keys);
		if (OidIsValid(state->hashers[key].fn_oid))
			hash_key_column(state, key, rows, keys);
		if (key == 0)
			check(state, (int8 ? state->kernels->int8_hash :
						  state->kernels->int4_hash) (keys, NULL, &batch->rows,
													  nulls,
													  state->hashes, valid,
													  &state->status));
		else
			check(state, (int8 ? state->kernels->int8_hash_next :
						  state->kernels->int4_hash_next) (keys, NULL,
														   nulls,
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
		for (int key = 0; key < state->nkeys; key++)
		{
			if (state->hash_values[key] != NULL)
			{
				pfree(state->hash_values[key]);
				pfree(state->hash_isnull[key]);
			}
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
	for (int key = 0; key < state->nkeys; key++)
	{
		if (!OidIsValid(state->hashers[key].fn_oid))
			continue;
		state->hash_values[key] = MemoryContextAllocZero(context, sizeof(Datum) * nrows);
		state->hash_isnull[key] = MemoryContextAllocZero(context, sizeof(bool) * nrows);
	}
	state->capacity = nrows;
}

/* The bytes the table, the copies of inner values and spilling take now. */
static Size
join_memory(TessHashJoinState *state)
{
	Size		memory = state->table_bytes + sizeof(uint64) * state->bloom_words +
		MemoryContextMemAllocated(state->values_context, true);

	if (state->spill != NULL)
		memory += spill_memory(state->spill, NULL);
	return memory;
}

static void
note_memory(TessHashJoinState *state)
{
	state->peak_memory = Max(state->peak_memory, join_memory(state));
}

/*
 * RIGHT and FULL: the table in memory is another, or is built anew: its
 * records have no mark, and its tail is still to come.
 */
static void
forget_marks(TessHashJoinState *state)
{
	if (state->marks_context != NULL)
		MemoryContextReset(state->marks_context);
	state->marks = NULL;
	state->mark_slots = 0;
	state->marks_shared = false;
	state->table_tail_done = false;
}

/* An atomic word of marks is a plain one: none simulated with a lock. */
StaticAssertDecl(sizeof(pg_atomic_uint64) == sizeof(uint64),
				 "TessHashJoin needs 64-bit atomics for shared marks");

/* The words of a chunk's marks in shared memory: a bit per record of the largest chunk. */
static Size
mark_words(TessHashJoinState *state)
{
	return ((TESS_TABLE_MAX_CHUNK_LEN - TESS_TABLE_CHUNK_HEADER) / state->record_size + 63) / 64;
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
	forget_marks(state);
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
 * The chunks of a table and of its values past the first: up to the
 * largest, and at most an eighth of hash_mem, so that a table spills only
 * near its limit; a shared table's participant's too.
 */
static Size
chunk_len_for(Size largest)
{
	return Max(JOIN_FIRST_CHUNK,
			   Min(largest, TYPEALIGN_DOWN(8, get_hash_memory_limit() / 8)));
}

/* Another chunk for the serial table, the last one being full. */
static void
add_table_chunk(TessHashJoinState *state)
{
	int			chunk = state->table.nchunks;
	Size		len = chunk == 0 ? JOIN_FIRST_CHUNK : chunk_len_for(JOIN_CHUNK_LEN);
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

/* The head of a participant's list of its chunks, or of its value chunks. */
static dsa_pointer *
participant_list(TessHashJoinState *state, int participant, bool values)
{
	dsa_pointer *heads = dsa_get_address(query_dsa(state), state->shared->lists);

	Assert(participant >= 0 && participant < state->shared->participants);
	return &heads[2 * participant + (values ? 1 : 0)];
}

static dsa_pointer *
own_list(TessHashJoinState *state, bool values)
{
	return participant_list(state, state->spill_participant, values);
}

/* The words of a shared table's spilling, mapped in this process. */
static uint64 *
shared_words(TessHashJoinState *state)
{
	if (state->spill_words == NULL)
		state->spill_words = dsa_get_address(query_dsa(state),
											 state->shared->spill_words);
	return state->spill_words;
}

/* The partitions of a shared table, 0 while it is whole. */
static uint32
shared_partitions(TessHashJoinState *state)
{
	uint32		partitions;

	check(state, state->kernels->table_spill_partitions(shared_words(state),
														state->shared->spill_nwords,
														&partitions,
														&state->status));
	return partitions;
}

/* Count bytes of this participant's chunks while the table is whole. */
static void
shared_count(TessHashJoinState *state, int64 delta)
{
	bool		over;

	check(state, state->kernels->table_spill_add_bytes(shared_words(state),
													   state->shared->spill_nwords,
													   delta, -1, &over,
													   &state->status));
	state->spill_over = over;
}

static bool
shared_on_disk(TessHashJoinState *state, uint32 partition)
{
	bool		on_disk;
	bool		alone;

	check(state, state->kernels->table_spill_flags(shared_words(state),
												   state->shared->spill_nwords,
												   partition, &on_disk, &alone,
												   &state->status));
	return on_disk;
}

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
		SpinLockRelease(&state->shared->lock);
		header->next = *own_list(state, true);
		*own_list(state, true) = block;
		header->number = number;
		header->len = len;
		header->owner = state->spill_participant;
		base = (char *) header + JOIN_CHUNK_HEADER;
		state->value_bytes = add_size(state->value_bytes, JOIN_CHUNK_HEADER);
		shared_count(state, (int64) (JOIN_CHUNK_HEADER + len));
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
				chunk_len_for(JOIN_VALUE_CHUNK);

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
		2 * chunk_len_for(JOIN_CHUNK_LEN) > get_hash_memory_limit())
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
/* This build's keys for the outer side's pruning: none seen yet. */
static void
reset_prune_keys(TessHashJoinState *state)
{
	state->prune_keys.rows = 0;
	state->prune_keys.min = PG_INT64_MAX;
	state->prune_keys.max = PG_INT64_MIN;
	state->prune_keys.nvalues = 0;
	state->prune_sent = false;
}

/*
 * The keys of an inner batch's rows for the outer side's pruning: the
 * lowest and the highest, and the keys themselves while the build has at
 * most JOIN_PRUNE_VALUES rows with one. A NULL key pairs with nothing.
 */
static void
note_prune_keys(TessHashJoinState *state, TessBatch *batch)
{
	TessJoinKeys *keys = &state->prune_keys;
	TessDatumColumn column;
	int			row = -1;

	if (!state->prune_on)
		return;
	child_column(batch, state->inner_keys[state->prune_key], &batch->rows,
				 TESS_COLUMN_FOR_FILTER, &column);
	while ((row = tess_row_mask_next(&batch->rows, row)) >= 0)
	{
		int64		value;

		if (column.isnull[row])
			continue;
		value = keys->int8 ? DatumGetInt64(column.values[row]) :
			(int64) DatumGetInt32(column.values[row]);
		keys->min = Min(keys->min, value);
		keys->max = Max(keys->max, value);
		keys->rows++;
		if (keys->nvalues >= 0 && keys->nvalues < JOIN_PRUNE_VALUES)
			keys->values[keys->nvalues++] = value;
		else
			keys->nvalues = -1;
	}
}

/*
 * A shared build: this participant's keys added to every participant's,
 * whole once the build's barrier is passed.
 */
static void
share_prune_keys(TessHashJoinState *state)
{
	TessJoinKeys *keys = &state->prune_keys;
	JoinShared *shared = state->shared;

	if (!state->prune_on || keys->rows == 0)
		return;
	SpinLockAcquire(&shared->prune_lock);
	shared->prune_rows += keys->rows;
	shared->prune_min = Min(shared->prune_min, keys->min);
	shared->prune_max = Max(shared->prune_max, keys->max);
	if (shared->prune_nvalues >= 0 &&
		(keys->nvalues < 0 || shared->prune_nvalues + keys->nvalues > JOIN_PRUNE_VALUES))
		shared->prune_nvalues = -1;
	else if (shared->prune_nvalues >= 0)
	{
		memcpy(&shared->prune_values[shared->prune_nvalues], keys->values,
			   sizeof(int64) * keys->nvalues);
		shared->prune_nvalues += keys->nvalues;
	}
	SpinLockRelease(&shared->prune_lock);
}

/*
 * The table built: the outer side's partitions its keys cannot pair with
 * are pruned before the outer node is read, once a build (a shared table's
 * keys are every participant's); a table kept over a rescan keeps them.
 */
static void
prune_outer(TessHashJoinState *state)
{
	if (!state->prune_on || state->prune_sent)
		return;
	if (state->shared != NULL)
	{
		JoinShared *shared = state->shared;
		TessJoinKeys *keys = &state->prune_keys;

		SpinLockAcquire(&shared->prune_lock);
		keys->rows = shared->prune_rows;
		keys->min = shared->prune_min;
		keys->max = shared->prune_max;
		keys->nvalues = shared->prune_nvalues;
		if (keys->nvalues > 0)
			memcpy(keys->values, shared->prune_values, sizeof(int64) * keys->nvalues);
		SpinLockRelease(&shared->prune_lock);
	}
	tess_append_join_prune(state->outer, &state->prune_keys);
	state->prune_sent = true;
}

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
	reset_prune_keys(state);
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->inner_input);

		if (batch == NULL)
			break;
		if (tess_row_mask_count(&batch->rows) > 0)
		{
			note_prune_keys(state, batch);
			insert_batch(state, batch);
		}
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
part_open(PartReader *reader, TessSpill *file, int partition, int writers)
{
	reader->file = file;
	reader->partition = partition;
	reader->writers = writers;
	reader->next = 0;
	reader->reader = NULL;
	reader->open = true;
}

/* The next block's header, from the next writer's file once one ends. */
static bool
part_header(PartReader *reader, TessSpillHeader *header)
{
	if (!reader->open)
		return false;
	for (;;)
	{
		if (reader->reader == NULL)
		{
			if (reader->next >= reader->writers)
			{
				reader->open = false;
				return false;
			}
			reader->reader = tess_spill_open(reader->file, reader->next++,
											 reader->partition);
			continue;
		}
		if (tess_spill_read_header(reader->reader, header))
			return true;
		tess_spill_close(reader->reader);
		reader->reader = NULL;
	}
}

static void
part_body(PartReader *reader, void *body, Size len)
{
	tess_spill_read_body(reader->reader, body, len);
}

static void
part_close(PartReader *reader)
{
	if (reader->reader != NULL)
		tess_spill_close(reader->reader);
	reader->reader = NULL;
	reader->open = false;
}

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

/* The partition of a hash on a side, which may have fewer partitions than its level. */
static inline uint32
side_partition(const SpillSide *side, const JoinSpill *spill, uint32 hash)
{
	return (hash >> spill->shift) & (uint32) (side->npartitions - 1);
}

static inline uint64
chunk_used(const void *base)
{
	return *(const uint64 *) base;
}

/* Whether a chunk of the side holds a row. */
static inline bool
side_chunk_rows(const SpillSide *side, const void *base)
{
	return side->columnar ? tess_spill_columns_rows(base) > 0 :
		chunk_used(base) > TESS_TABLE_CHUNK_HEADER;
}

/* Make the len bytes at base an empty chunk of the side: of records, or of columns. */
static void
side_chunk_init(TessHashJoinState *state, SpillSide *side, void *base, Size len)
{
	Size		capacity;

	if (side->columnar)
		check(state, state->kernels->spill_columns_init(base, len, side->nwords,
														&capacity, &state->status));
	else
		check(state, state->kernels->table_chunk_init(base, len, &state->status));
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
		  bool resident, int npartitions)
{
	JoinSpill  *spill = state->spill;
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);
	void	   *empty;
	Size		empty_len;

	/*
	 * Small blocks: a chunk past a few kB gets a block of its own, of its
	 * size, not a share of a block twice as large, and memory is what the
	 * chunks take.
	 */
	if (resident)
		side->context = AllocSetContextCreate(spill->context,
											  "TessHashJoin inner partitions",
											  ALLOCSET_SMALL_SIZES);
	else
		side->context = AllocSetContextCreate(spill->context,
											  "TessHashJoin outer partitions",
											  ALLOCSET_SMALL_SIZES);
	side->columnar = !resident;
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
	side->pointers = MemoryContextAllocZero(side->context, sizeof(dsa_pointer) * side->slots);
	empty_len = side->columnar ? TESS_SPILL_COLUMNS_HEADER : TESS_TABLE_CHUNK_HEADER;
	empty = MemoryContextAlloc(side->context, empty_len);
	side_chunk_init(state, side, empty, empty_len);
	side->bases[0] = empty;
	side->lens[0] = empty_len;
	side->nchunks = 1;
	side_sync(side);
	side->npartitions = npartitions;
	side->current = MemoryContextAllocZero(side->context,
										   sizeof(uint32) * npartitions);
	side->parts = MemoryContextAllocZero(side->context,
										 sizeof(SpillPart) * npartitions);
	side->rows = MemoryContextAllocZero(side->context, sizeof(uint64) * npartitions);
	for (int partition = 0; partition < npartitions; partition++)
	{
		side->parts[partition].value_current = -1;
		side->parts[partition].resident = resident;
	}
	side->queue = MemoryContextAlloc(side->context, sizeof(int) * npartitions);
	check(state, state->kernels->table_fingerprint(&side->ref, &side->fingerprint,
												   &state->status));
	config.parent_context = side->context;
	config.kernels = state->kernels;
	config.npartitions = npartitions;
	config.level = spill->level;
	config.fingerprint = side->fingerprint;
	config.max_len = (uint64) MaxAllocHugeSize;
	config.buffer_len = TESS_SPILL_BUFFER_LEN(get_hash_memory_limit());
	side->file = tess_spill_create(&config);
}

/*
 * Count bytes of a shared side's chunks in the words of the table's
 * spilling, of a partition: whether they pass the budget is kept.
 */
static void
side_count(SpillSide *side, int partition, int64 delta)
{
	TessStatus	status = TESS_STRUCT_INITIALIZER(TessStatus);

	if (side->area == NULL)
		return;
	if (side->kernels->table_spill_add_bytes(side->spill_words, side->spill_nwords,
											 delta, partition, &side->over,
											 &status) != TESS_OK)
		tess_status_report(&status);
}

/* The bytes a chunk of len bytes takes: with its header when shared. */
static inline int64
side_block(SpillSide *side, Size len)
{
	return (int64) (side->area != NULL ? JOIN_CHUNK_HEADER + len : len);
}

/* What a chunk of records of len bytes costs a shared side's budget, its index and filter included. */
static inline int64
side_chunk_cost(SpillSide *side, Size len)
{
	return record_chunk_cost(len, TYPEALIGN(8, 16 + 8 * side->nkeys + side->payload_size));
}

/* A block of len bytes: in the query's shared memory after a header, or the side's own. */
static void *
side_alloc(SpillSide *side, Size len, dsa_pointer *pointer)
{
	JoinChunk  *header;

	if (side->area == NULL)
	{
		*pointer = InvalidDsaPointer;
		return MemoryContextAllocExtended(side->context, len, MCXT_ALLOC_HUGE);
	}
	*pointer = dsa_allocate_extended(side->area, JOIN_CHUNK_HEADER + len, DSA_ALLOC_HUGE);
	header = dsa_get_address(side->area, *pointer);
	header->next = InvalidDsaPointer;
	header->number = 0;
	header->len = len;
	header->owner = side->owner;
	return (char *) header + JOIN_CHUNK_HEADER;
}

static void
side_free(SpillSide *side, void *base, dsa_pointer pointer)
{
	if (DsaPointerIsValid(pointer))
		dsa_free(side->area, pointer);
	else
		pfree(base);
}

/* Free chunk `index` of a partition, and count it. */
static void
side_free_chunk(SpillSide *side, int partition, int index)
{
	side_free(side, side->bases[index], side->pointers[index]);
	side->bases[index] = NULL;
	side->pointers[index] = InvalidDsaPointer;
	side->bytes -= side->lens[index];
	side->parts[partition].bytes -= side->lens[index];
	side_count(side, partition, -side_chunk_cost(side, side->lens[index]));
}

/* Free value chunk `number` of a partition, and count it. */
static void
side_free_values(SpillSide *side, int partition, int number)
{
	side_free(side, side->value_bases[number], side->value_pointers[number]);
	side->value_bases[number] = NULL;
	side->value_pointers[number] = InvalidDsaPointer;
	side->bytes -= side->value_allocated[number];
	side->parts[partition].bytes -= side->value_allocated[number];
	side_count(side, partition, -side_block(side, side->value_allocated[number]));
}

/* Room for value chunk `number`, a number of the side or of every participant. */
static void
side_value_slot(SpillSide *side, int number)
{
	int			slots = Max(side->value_slots, 16);

	if (number < side->value_slots)
		return;
	while (slots <= number)
		slots *= 2;
	side->value_bases = side->value_bases == NULL ?
		MemoryContextAllocZero(side->context, sizeof(char *) * slots) :
		repalloc0(side->value_bases, sizeof(char *) * side->value_slots,
				  sizeof(char *) * slots);
	side->value_pointers = side->value_pointers == NULL ?
		MemoryContextAllocZero(side->context, sizeof(dsa_pointer) * slots) :
		repalloc0(side->value_pointers, sizeof(dsa_pointer) * side->value_slots,
				  sizeof(dsa_pointer) * slots);
	side->value_lens = side->value_lens == NULL ?
		MemoryContextAllocZero(side->context, sizeof(Size) * slots) :
		repalloc0(side->value_lens, sizeof(Size) * side->value_slots,
				  sizeof(Size) * slots);
	side->value_allocated = side->value_allocated == NULL ?
		MemoryContextAllocZero(side->context, sizeof(Size) * slots) :
		repalloc0(side->value_allocated, sizeof(Size) * side->value_slots,
				  sizeof(Size) * slots);
	side->value_slots = slots;
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
		side->pointers = repalloc0(side->pointers, sizeof(dsa_pointer) * side->slots,
								   sizeof(dsa_pointer) * side->slots * 2);
		side->slots *= 2;
		side->bases = repalloc(side->bases, sizeof(void *) * side->slots);
		side->lens = repalloc(side->lens, sizeof(Size) * side->slots);
	}
	base = side_alloc(side, side->chunk_len, &side->pointers[index]);
	side_chunk_init(state, side, base, side->chunk_len);
	side->bases[index] = base;
	side->lens[index] = side->chunk_len;
	side->nchunks++;
	side_sync(side);
	grow_ints(side->context, &part->chunks, &part->chunk_slots, part->nchunks + 1);
	part->chunks[part->nchunks++] = index;
	side->current[partition] = index;
	part->bytes += side->chunk_len;
	side->bytes += side->chunk_len;
	side_count(side, partition, side_chunk_cost(side, side->chunk_len));
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
		side->pointers[kept] = side->pointers[index];
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
	/* disk_bytes: what the block takes read back; the disk counts what was stored. */
	state->counters[JOIN_DISK] += tess_spill_write(side->file, partition, kind, number,
												   body, len, NULL);
	side->parts[partition].written = true;
	side->parts[partition].disk_bytes += len;
	if (kind != TESS_SPILL_VALUES)
		side->parts[partition].blocks++;
	state->counters[JOIN_SPILLED]++;
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
		side_free_values(side, partition, number);
	}
	part->nvalues = 0;
	part->value_current = -1;
	part->value_bytes = 0;
}

/* Write chunk `index` of the partition, unless it holds no row. */
static void
side_write_records(TessHashJoinState *state, SpillSide *side, int partition,
				   int index)
{
	if (!side_chunk_rows(side, side->bases[index]))
		return;
	if (side->columnar)
		write_block(state, side, partition, TESS_SPILL_COLUMNS,
					side->next_number++, side->bases[index], side->lens[index]);
	else
		write_block(state, side, partition, TESS_SPILL_RECORDS,
					side->next_number++, side->bases[index],
					chunk_used(side->bases[index]));
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
	side_chunk_init(state, side, side->bases[index], side->lens[index]);
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
		side_free_chunk(side, partition, index);
	}
	if (tail != 0)
		side_chunk_init(state, side, side->bases[tail], side->lens[tail]);
	side_compact(side);
}

/*
 * A partition on disk writes its values and its tail and frees them, as
 * its level starts joining: the tails of every partition kept in memory
 * would leave a partition, or a level below, little of hash_mem, and a
 * small hash_mem split partitions thousands of times.
 */
static void
side_evict(TessHashJoinState *state, SpillSide *side, int partition)
{
	int			index = side->current[partition];

	if (side->parts[partition].resident)
		return;
	side_write_values(state, side, partition);
	side->parts[partition].queued = false;
	if (index == 0)
		return;
	side_write_records(state, side, partition, index);
	side_free_chunk(side, partition, index);
	side->current[partition] = 0;
}

/* A value chunk of len bytes for the partition; its number. */
static int
side_value_chunk(SpillSide *side, int partition, Size len)
{
	SpillPart  *part = &side->parts[partition];
	int			number = side->nvalues;

	/* A shared table's value chunks take numbers every participant shares. */
	if (side->area != NULL)
	{
		SpinLockAcquire(&side->shared->lock);
		number = (int) side->shared->next_value_chunk++;
		SpinLockRelease(&side->shared->lock);
	}
	if (number >= INT_MAX - 1)
		ereport(ERROR,
				(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
				 errmsg("TessHashJoin cannot hold more chunks of values")));
	side_value_slot(side, number);
	side->value_bases[number] = side_alloc(side, len, &side->value_pointers[number]);
	if (side->area != NULL)
		((JoinChunk *) (side->value_bases[number] - JOIN_CHUNK_HEADER))->number = number;
	side->value_lens[number] = 0;
	side->value_allocated[number] = len;
	side->nvalues = Max(side->nvalues, number + 1);
	side_count(side, partition, side_block(side, len));
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
	Size		bytes = 0;

	for (int partition = 0; partition < spill->npartitions; partition++)
		if (spill->build.parts[partition].resident)
			rows += spill->build.rows[partition];
	if (resident != NULL)
		*resident = rows;
	/*
	 * The levels above keep the tails of their partitions to come; every
	 * set keeps its write buffer while it writes, and its readers a block.
	 */
	for (JoinSpill *level = spill; level != NULL; level = level->parent)
	{
		bytes += level->build.bytes + level->probe.bytes +
			sizeof(uint64) * level->bloom_words +
			MemoryContextMemAllocated(level->part_context, true) +
			MemoryContextMemAllocated(level->block_context, true) +
			tess_spill_memory(level->build.file) + tess_spill_memory(level->probe.file);
	}
	return bytes + (spill->indexed ? 0 : rows * sizeof(uint64));
}

/*
 * Keep spilling within hash_mem while building: while it takes more, the
 * largest resident partition goes to disk. Room stays for the outer
 * side's tails of the partitions on disk, which come once the probing
 * starts: a chunk of records, one of values when the outer side keeps a
 * by-reference column, and a file's buffer each; a resident partition
 * writes no outer row. The tails stay: the chunk size bounds them, and
 * writing them sooner would write chunks of a few rows.
 */
static void
make_room(TessHashJoinState *state, bool building)
{
	JoinSpill  *spill = state->spill;
	Size		limit = get_hash_memory_limit();
	Size		tail = spill->probe.chunk_len + BLCKSZ;
	int			on_disk = 0;

	for (int word = 0; word < spill->probe.nwords; word++)
		if (!spill->probe.byvals[word])
		{
			tail += spill->probe.chunk_len;
			break;
		}
	for (int partition = 0; partition < spill->npartitions; partition++)
		if (!spill->build.parts[partition].resident)
			on_disk++;
	/* What the node reports, so that what it keeps is what it says. */
	while (building && join_memory(state) + on_disk * tail > limit)
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
		on_disk++;
	}
	/*
	 * Too little left resident to be worth probing, as finish_spill_build
	 * decides: all of it goes now, not after the build.
	 */
	if (building && on_disk > 0)
	{
		uint64		resident = 0;

		for (int partition = 0; partition < spill->npartitions; partition++)
			if (spill->build.parts[partition].resident)
				resident += spill->build.rows[partition];
		if (resident > 0 && resident * 4 < spill->total_rows)
			for (int partition = 0; partition < spill->npartitions; partition++)
				if (spill->build.parts[partition].resident)
					side_demote(state, &spill->build, partition);
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

/*
 * A level of partitions by the hash bits from shift, for an inner side
 * of expected bytes, as the current one: the power of two of them that
 * makes each about half of hash_mem, as long as the bits last and each
 * partition's tails on both sides fit in half of hash_mem.
 */
static JoinSpill *
spill_create(TessHashJoinState *state, JoinSpill *parent, double expected,
			 uint32 shift, int forced)
{
	MemoryContext context = state->css.ss.ps.state->es_query_cxt;
	Size		limit = get_hash_memory_limit();
	Size		record = 16 + 8 * state->nkeys + sizeof(uint64) * (1 + state->npayload);
	Size		chunk_len;
	int			npartitions = JOIN_SPILL_MIN_PARTITIONS;
	JoinSpill  *spill;
	int16	   *typlens;
	bool	   *byvals;
	int			nchild = 0;
	int			nstored = 0;

	/* A level below has what the levels above leave, a quarter at least. */
	if (parent != NULL)
	{
		Size		used = spill_memory(parent, NULL);

		limit = used < limit / 4 * 3 ? limit - used : limit / 4;
	}

	/*
	 * Each partition keeps a tail of records and one of values and a
	 * file's buffer of a page on each side: at the smallest chunk, half of
	 * hash_mem bounds them all.
	 */
	while (npartitions < JOIN_SPILL_MAX_PARTITIONS &&
		   (double) npartitions * (limit / 2) < expected &&
		   (Size) npartitions * 2 * (4 * JOIN_SPILL_MIN_CHUNK + 2 * BLCKSZ) <= limit / 2 &&
		   shift + pg_leftmost_one_pos32(npartitions) + 1 < 32)
		npartitions *= 2;
	/* A shared table's partitions, which every participant took. */
	if (forced > 0)
		npartitions = forced;
	chunk_len = limit / (16 * npartitions);
	chunk_len = Min(chunk_len, JOIN_CHUNK_LEN);
	chunk_len = Max(chunk_len, JOIN_SPILL_MIN_CHUNK);
	chunk_len = Max(chunk_len, TESS_TABLE_CHUNK_HEADER + 4 * record);
	chunk_len = TYPEALIGN_DOWN(8, chunk_len);

	spill = MemoryContextAllocZero(context, sizeof(JoinSpill));
	state->spill = spill;
	spill->parent = parent;
	spill->level = parent == NULL ? 0 : parent->level + 1;
	spill->context = AllocSetContextCreate(context, "TessHashJoin spill",
										   ALLOCSET_DEFAULT_SIZES);
	spill->part_context = AllocSetContextCreate(spill->context,
												"TessHashJoin partition",
												ALLOCSET_SMALL_SIZES);
	spill->block_context = AllocSetContextCreate(spill->context,
												 "TessHashJoin outer block",
												 ALLOCSET_SMALL_SIZES);
	spill->npartitions = npartitions;
	spill->shift = shift;
	spill->partition = -1;
	/* A level's own files; a shared table's first level reads every participant's. */
	spill->writers = 1;
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
			  byvals, chunk_len, true, npartitions);

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
			  chunk_len, false, npartitions);
	/* The outer rows a shared table answers: one partition of their own. */
	if (state->shared != NULL && parent == NULL)
		side_init(state, &spill->resident, state->nkeys, state->outer_kinds, nstored,
				  typlens, byvals, chunk_len, false, 1);
	spill->rows = &spill->probe;
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
	spill->window = MemoryContextAllocZero(spill->context, sizeof(Datum *) * Max(nstored, 1));
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
	spill->batch.private_data = spill;
	spill->batch.rows.bits = spill->bits;
	return spill;
}

/*
 * The table outgrew hash_mem: the first level of partitions, for twice
 * what was read, or what the planner expects if more; a Bloom filter of
 * every inner row, and the table built so far moved into the partitions.
 */
static void
start_spill(TessHashJoinState *state)
{
	double		bytes = (double) state->table_bytes + state->value_bytes;
	double		expected = bytes * 2;
	JoinSpill  *spill;
	uint64		expected_rows;

	if (state->build_rows > 0)
		expected = Max(expected, bytes / state->build_rows * state->inner_rows);
	spill = spill_create(state, NULL, expected, 0, 0);

	/* A filter of every inner row, sized for the rows expected. */
	state->counters[JOIN_BLOOM_FILTERS]++;
	expected_rows = Max((uint64) state->inner_rows, state->build_rows * 2);
	check(state, state->kernels->table_bloom_words(Max(expected_rows, 1),
												   &spill->bloom_words,
												   &state->status));
	/*
	 * An eighth of hash_mem at most: a smaller filter lets more rows
	 * through, a larger one would leave a small hash_mem no room.
	 */
	while (spill->bloom_words > 1 &&
		   sizeof(uint64) * spill->bloom_words > get_hash_memory_limit() / 8)
		spill->bloom_words /= 2;
	spill->bloom = MemoryContextAllocExtended(spill->context,
											  mul_size(sizeof(uint64), spill->bloom_words),
											  MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
	split_table(state);
}

/* Room for the appends of a batch of the node's capacity, of either side. */
static void
spill_reserve(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	if (spill->payload_rows >= state->capacity)
		return;
	if (spill->before_bits != NULL)
	{
		pfree(spill->before_bits);
		pfree(spill->payloads);
	}
	spill->before_bits = MemoryContextAlloc(spill->context,
											sizeof(uint64) *
											tess_row_mask_word_count(state->capacity));
	spill->payloads = MemoryContextAlloc(spill->context,
										 sizeof(uint8 *) * state->capacity);
	spill->payload_rows = state->capacity;
}

/*
 * The table built before the first spill, into the partitions: its
 * chunks one by one, each split by the kernel into the partitions'
 * chunks, the by-reference values of its records copied into the value
 * chunks of their partitions, and then freed.
 */
/*
 * Split the len bytes at base, a chunk of records of the inner side's
 * layout, into the current level's partitions: each record copied by the
 * kernel to its partition's chunk, its by-reference values, found through
 * values (the bases of the value chunks it refers to), copied into its
 * partition's value chunks, and its hash into the level's Bloom filter.
 */
static void
split_chunk(TessHashJoinState *state, void *base, Size len, char *const *values)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	uint32		offsets[JOIN_COMPACT_ROWS];
	uint32		hashes[JOIN_COMPACT_ROWS];
	uint8	   *payloads[JOIN_COMPACT_ROWS];
	bool		byref = false;
	Size		from = TESS_TABLE_CHUNK_HEADER;
	int			source;

	for (int word = 0; word < side->nwords; word++)
		byref |= !side->byvals[word];
	/* The chunk joins the side's for the call, as a chunk of no partition. */
	if (side->nchunks == side->slots)
	{
		side->pointers = repalloc0(side->pointers, sizeof(dsa_pointer) * side->slots,
								   sizeof(dsa_pointer) * side->slots * 2);
		side->slots *= 2;
		side->bases = repalloc(side->bases, sizeof(void *) * side->slots);
		side->lens = repalloc(side->lens, sizeof(Size) * side->slots);
	}
	source = side->nchunks++;
	side->bases[source] = base;
	side->lens[source] = len;
	side->pointers[source] = InvalidDsaPointer;
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
		if (count > 0 && spill->bloom != NULL)
			check(state, (spill->shared ? state->kernels->bloom_shared_add :
						  state->kernels->bloom_add) (spill->bloom, spill->bloom_words,
													  hashes,
													  &(TessRowMask) {count, &bits},
													  &state->status));
		/* The payloads of the records just split, in one call. */
		if (byref && count > 0)
			check(state, state->kernels->table_payloads(&side->ref, offsets,
														&(TessRowMask) {count, &bits},
														payloads, &state->status));
		for (int index = 0; index < count; index++)
		{
			int			partition = spill_partition(spill, hashes[index]);

			side->rows[partition]++;
			if (!byref)
				continue;
			for (int word = 0; word < side->nwords; word++)
			{
				uint64	   *slot = (uint64 *) payloads[index] + 1 + word;
				char	   *value;

				if (side->byvals[word] || *slot == 0)
					continue;
				if (values[(*slot >> 32) - 1] == NULL)
					elog(ERROR, "TessHashJoin split a record whose values it does not have");
				value = values[(*slot >> 32) - 1] + (*slot & 0xFFFFFFFF);
				*slot = side_store(side, partition, PointerGetDatum(value),
								   side->typlens[word]);
			}
		}
		if (full >= 0)
		{
			if (side->parts[full].resident || side->current[full] == 0)
				side_add_chunk(state, side, full);
			else
				side_flush(state, side, full);
		}
		else if (count == 0)
			break;
	}
	/* The chunk leaves the side; its memory is the caller's. */
	side->bases[source] = NULL;
	side_compact(side);
}

static void
split_table(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	for (int chunk = 0; chunk < state->table.nchunks; chunk++)
	{
		split_chunk(state, state->chunk_bases[chunk], state->chunk_lens[chunk],
					state->value_bases);
		pfree(state->chunk_bases[chunk]);
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
		int			partition = side_partition(side, spill, hashes[row]);
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
 * side_append of the outer side: the rows of pending into the chunks of
 * columns of their partitions, the words of columns (by-reference ones
 * as pointers first); a by-reference value is then copied into its
 * partition's value chunks, as a record's is, and its word refers to the
 * copy. Rows go on as the partitions' chunks fill; pending ends empty.
 */
static void
column_append(TessHashJoinState *state, SpillSide *side, TessRowMask *pending,
			  const TessDatumColumn *columns, bool byref)
{
	JoinSpill  *spill = state->spill;
	int			nwords = tess_row_mask_word_count(pending->nrows);
	TessRowMask before = {pending->nrows, spill->before_bits};

	for (;;)
	{
		if (byref)
			memcpy(spill->before_bits, pending->bits, sizeof(uint64) * nwords);
		check(state, state->kernels->spill_columns_append_partitioned((void *const *) side->bases,
																	  side->lens, side->nchunks,
																	  side->current,
																	  side->npartitions,
																	  spill->shift,
																	  state->hashes,
																	  side->nwords, columns,
																	  pending, state->offsets,
																	  side->rows,
																	  &state->status));
		if (byref)
		{
			int			row = -1;

			for (int word = 0; word < nwords; word++)
				spill->before_bits[word] &= ~pending->bits[word];
			while ((row = tess_row_mask_next(&before, row)) >= 0)
			{
				uint32		place = state->offsets[row];
				void	   *base = side->bases[place >> TESS_SPILL_COLUMNS_PLACE_BITS];
				int			partition = side_partition(side, spill, state->hashes[row]);

				place &= (1u << TESS_SPILL_COLUMNS_PLACE_BITS) - 1;
				for (int word = 0; word < side->nwords; word++)
				{
					if (side->byvals[word] || columns[word].isnull[row])
						continue;
					tess_spill_columns_lane(base, 1 + word)[place] =
						side_store(side, partition, columns[word].values[row],
								   side->typlens[word]);
				}
			}
		}
		if (tess_row_mask_count(pending) == 0)
			break;
		make_chunks(state, side, pending, state->hashes);
	}
	side_flush_queue(state, side);
}

/*
 * Append the rows of pending, hashed and keyed by batch_keys, as records
 * of the side's partitions: the payload is the NULL bits and a word per
 * column of the batch at children, which the kernel takes as columns,
 * counting the rows of each partition; a by-reference value is copied
 * into its partition's value chunks right after its row went in, its
 * word then referring to the copy, so that the values of a chunk's rows
 * are written with it or before it, never with the chunk before. Rows go
 * on as the partitions' chunks fill; pending ends empty. With nulls, the
 * NULL bits of the columns are kept there.
 */
static void
side_append(TessHashJoinState *state, SpillSide *side, TessBatch *batch,
			TessRowMask *pending, const int *children, uint64 *nulls)
{
	JoinSpill  *spill = state->spill;
	int			nwords = tess_row_mask_word_count(pending->nrows);
	TessRowMask before = {pending->nrows, NULL};
	TessDatumColumn *columns = spill->columns;
	uint64		seen = 0;
	bool		byref = false;

	for (int word = 0; word < side->nwords; word++)
		byref |= !side->byvals[word];
	spill_reserve(state);
	before.bits = spill->before_bits;
	for (int word = 0; word < side->nwords; word++)
		child_column(batch, children[word], pending, TESS_COLUMN_FOR_PROJECTION,
					 &columns[word]);
	if (side->columnar)
	{
		column_append(state, side, pending, columns, byref);
		return;
	}
	for (;;)
	{
		if (byref)
			memcpy(spill->before_bits, pending->bits, sizeof(uint64) * nwords);
		check(state, state->kernels->table_append_partitioned_columns(&side->ref, side->current,
																	  side->npartitions,
																	  spill->shift,
																	  state->hashes, state->nkeys,
																	  state->table_keys,
																	  side->nwords, columns,
																	  pending, state->offsets,
																	  side->rows, &seen,
																	  &state->status));
		/* The by-reference values of the rows just appended, found through their payloads in one call. */
		if (byref)
		{
			int			row = -1;

			for (int word = 0; word < nwords; word++)
				spill->before_bits[word] &= ~pending->bits[word];
			if (tess_row_mask_count(&before) > 0)
				check(state, state->kernels->table_payloads(&side->ref, state->offsets, &before,
															spill->payloads, &state->status));
			while ((row = tess_row_mask_next(&before, row)) >= 0)
			{
				int			partition = side_partition(side, spill, state->hashes[row]);

				for (int word = 0; word < side->nwords; word++)
				{
					if (side->byvals[word] || columns[word].isnull[row])
						continue;
					((uint64 *) spill->payloads[row])[1 + word] =
						side_store(side, partition, columns[word].values[row],
								   side->typlens[word]);
				}
			}
		}
		if (tess_row_mask_count(pending) == 0)
			break;
		make_chunks(state, side, pending, state->hashes);
	}
	if (nulls != NULL)
		*nulls |= seen;
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
	check(state, (spill->shared ? state->kernels->bloom_shared_add :
				  state->kernels->bloom_add) (spill->bloom, spill->bloom_words,
											  state->hashes, &valid, &state->status));
	memcpy(state->pending_bits, state->valid_bits, sizeof(uint64) * nwords);
	side_append(state, &spill->build, batch, &pending, spill->build_children,
				&state->null_columns);
	state->build_rows += count;
	spill->total_rows += count;
	state->counters[JOIN_BUILD_ROWS] += count;
	/* A shared table's partitions go to disk as every participant decides. */
	if (spill->shared)
		state->appended += count;
	else
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
	uint64		resident = 0;

	/*
	 * Resident partitions holding less than a quarter of the inner rows go
	 * to disk too: probing them would cost every outer batch the whole
	 * probe for the few rows of theirs, more than writing them saves.
	 */
	for (int partition = 0; partition < spill->npartitions; partition++)
		if (side->parts[partition].resident)
			resident += side->rows[partition];
	if (resident > 0 && resident * 4 < spill->total_rows)
		for (int partition = 0; partition < spill->npartitions; partition++)
			if (side->parts[partition].resident)
				side_demote(state, side, partition);

	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		SpillPart  *part = &side->parts[partition];

		if (!part->resident)
			continue;
		state->counters[JOIN_RESIDENT]++;
		rows += side->rows[partition];
		reserve_chunks(state, nchunks + part->nchunks);
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			state->chunk_bases[nchunks] = side->bases[part->chunks[chunk]];
			state->chunk_lens[nchunks] = side->lens[part->chunks[chunk]];
			nchunks++;
		}
	}
	reserve_chunks(state, Max(nchunks, 1));
	if (spill->parent == NULL)
		spill->input_rows = spill->total_rows;
	state->table.index = NULL;
	state->table.nchunks = nchunks;
	/* A level's own resident table, with its tail still to come. */
	forget_marks(state);
	/* The chunks count with the side's memory; the index with the table's. */
	state->table_bytes = 0;
	state->build_rows = rows;
	state->value_bases = side->value_bases;
	state->nvalue_chunks = side->nvalues;
	spill->indexed = true;
	if (spill->parent == NULL)
		index_table(state);
	else
	{
		uint64		buckets = state->counters[JOIN_BUCKETS];

		/* The buckets count for the build, not for a level below. */
		index_table(state);
		state->counters[JOIN_BUCKETS] = buckets;
	}
}

/* Free a partition's chunks and value chunks in memory of one side. */
static void
side_forget(SpillSide *side, int partition)
{
	SpillPart  *part = &side->parts[partition];

	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		int			index = part->chunks[chunk];

		if (side->bases[index] != NULL && index != 0)
			side_free_chunk(side, partition, index);
	}
	part->nchunks = 0;
	side->current[partition] = 0;
	for (int index = 0; index < part->nvalues; index++)
	{
		int			number = part->values[index];

		if (side->value_bases[number] != NULL)
			side_free_values(side, partition, number);
	}
	part->nvalues = 0;
	part->value_current = -1;
	part->bytes = 0;
}

/* Free a partition's tail and value chunks of one side, and its files. */
static void
side_release(SpillSide *side, int partition)
{
	side_forget(side, partition);
	if (!side->shared_files)
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
	if (spill->bloom != NULL)
		pfree(spill->bloom);
	spill->bloom = NULL;
	spill->bloom_words = 0;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		if (spill->build.parts[partition].resident)
			side_release(&spill->build, partition);
		else
			side_evict(state, &spill->build, partition);
		side_evict(state, &spill->probe, partition);
	}
	side_compact(&spill->build);
	side_compact(&spill->probe);
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	forget_marks(state);
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
	if (state->round_partition >= 0)
		round_leave(state);
	part_close(&spill->reader);
	part_close(&spill->build_reader);
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
	forget_marks(state);
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
	forget_marks(state);
}

/* A value chunk read back into the piece's table. */
static void
add_loaded_values(JoinSpill *spill, uint32 number, void *body)
{
	/* Another participant's number may be past this one's. */
	if (number >= INT_MAX - 1)
		elog(ERROR, "TessHashJoin read back a value chunk it never wrote");
	side_value_slot(&spill->build, (int) number);
	spill->build.nvalues = Max(spill->build.nvalues, (int) number + 1);
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
	while (part_header(&spill->build_reader, &header))
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
			part_body(&spill->build_reader, spill->carried, header.len);
			break;
		}
		body = MemoryContextAllocExtended(spill->part_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		part_body(&spill->build_reader, body, header.len);
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
		part_close(&spill->build_reader);
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

	part_open(&spill->reader, spill->probe.file, spill->partition, spill->writers);
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

	/* A round's outer rows: the next file this participant takes. */
	if (state->round_partition >= 0)
		return round_next_outer(state);
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
 * Split partition `partition` of the current level into a level below:
 * its inner rows read back group by group (value chunks, then the chunks
 * of records that refer to them), and then its tail, each chunk split by
 * the hash bits above the level's into the new level's partitions, which
 * start resident and go to disk as memory runs short, as the first
 * level's did. The new level then probes with the partition's outer
 * rows.
 */
static void
start_level(TessHashJoinState *state, int partition)
{
	JoinSpill  *parent = state->spill;
	SpillSide  *from = &parent->build;
	SpillPart  *part = &from->parts[partition];
	TessSpillHeader header;
	JoinSpill  *spill;
	bool		records = false;

	spill = spill_create(state, parent, (double) part->disk_bytes + part->bytes,
						 parent->shift + pg_leftmost_one_pos32(parent->npartitions), 0);
	spill->input_rows = from->rows[partition];
	spill->total_rows = from->rows[partition];
	state->counters[JOIN_SPLITS]++;
	state->counters[JOIN_BATCHES] = Max(state->counters[JOIN_BATCHES],
										(uint64) spill->npartitions);
	while (part_header(&parent->build_reader, &header))
	{
		void	   *body;

		if (header.kind == TESS_SPILL_VALUES)
		{
			/* A new group: the values of the one before are done. */
			if (records)
			{
				for (int index = 0; index < parent->nloaded; index++)
					from->value_bases[parent->loaded_values[index]] = NULL;
				parent->nloaded = 0;
				MemoryContextReset(spill->part_context);
				records = false;
			}
			body = MemoryContextAllocExtended(spill->part_context, Max(header.len, 8),
											  MCXT_ALLOC_HUGE);
			part_body(&parent->build_reader, body, header.len);
			add_loaded_values(parent, header.number, body);
			continue;
		}
		body = MemoryContextAllocExtended(spill->block_context, Max(header.len, 8),
										  MCXT_ALLOC_HUGE);
		part_body(&parent->build_reader, body, header.len);
		split_chunk(state, body, header.len, from->value_bases);
		MemoryContextReset(spill->block_context);
		records = true;
		make_room(state, true);
	}
	part_close(&parent->build_reader);
	for (int chunk = 0; chunk < part->nchunks; chunk++)
	{
		int			index = part->chunks[chunk];

		if (chunk_used(from->bases[index]) > TESS_TABLE_CHUNK_HEADER)
			split_chunk(state, from->bases[index], from->lens[index], from->value_bases);
	}
	for (int index = 0; index < parent->nloaded; index++)
		from->value_bases[parent->loaded_values[index]] = NULL;
	parent->nloaded = 0;
	MemoryContextReset(spill->part_context);
	side_release(from, partition);
	make_room(state, true);
	finish_spill_build(state);
}

/* Delete a level's files and free its memory. */
static void
level_free(JoinSpill *spill)
{
	SpillSide  *build = &spill->build;

	part_close(&spill->reader);
	part_close(&spill->build_reader);
	/* A shared side's chunks still its own go back to the shared memory. */
	if (build->area != NULL)
	{
		for (int index = 0; index < build->nchunks; index++)
			if (DsaPointerIsValid(build->pointers[index]))
				dsa_free(build->area, build->pointers[index]);
		for (int number = 0; number < build->value_slots; number++)
			if (DsaPointerIsValid(build->value_pointers[number]))
				dsa_free(build->area, build->value_pointers[number]);
	}
	tess_spill_release(spill->build.file);
	tess_spill_release(spill->probe.file);
	if (spill->resident.file != NULL)
		tess_spill_release(spill->resident.file);
	MemoryContextDelete(spill->context);
	pfree(spill);
}

/* A level is done: the level whose partition it split goes on. */
static void
pop_level(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	state->spill = spill->parent;
	level_free(spill);
	MemoryContextReset(state->table_context);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->build_rows = 0;
	state->duplicates = 0;
}

/*
 * Load a partition on disk to join: split into a level below when too
 * large and no single key, in pieces when still too large, whole
 * otherwise; its outer rows read from their first.
 */
static bool
open_partition(TessHashJoinState *state, int partition)
{
	JoinSpill  *spill = state->spill;

	spill->partition = partition;
	part_open(&spill->build_reader, spill->build.file, partition, spill->writers);
	spill->pieces_done = false;
	if (spill->build.parts[partition].disk_bytes > spill_room(state) &&
		spill->shift + pg_leftmost_one_pos32(spill->npartitions) + 2 <= 32 &&
		spill->build.rows[partition] < spill->input_rows / 10 * 9)
	{
		/*
		 * Too large, and much smaller than what this level split, so not
		 * one key: it splits by the next bits into a level below, whose
		 * outer rows are the partition's.
		 */
		open_outer_rows(state);
		start_level(state, partition);
		note_memory(state);
		return true;
	}
	/* The file larger than the room: pieces, and passes over the outer rows. */
	spill->multipass = spill->build.parts[partition].disk_bytes > spill_room(state);
	if (spill->multipass && state->jointype != JOIN_INNER)
	{
		spill->matched_words = Max((spill->probe.rows[partition] + 63) / 64, 1);
		spill->matched_rows =
			MemoryContextAllocExtended(spill->context,
									   sizeof(uint64) * spill->matched_words,
									   MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
	}
	load_piece(state, partition, !spill->multipass);
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
	if (spill->shared)
		return shared_next_partition(state);
	for (int partition = spill->partition + 1; partition < spill->npartitions; partition++)
	{
		if (spill->build.parts[partition].resident)
			continue;
		/* RIGHT and FULL return a partition's inner rows without outer ones too. */
		if (spill->probe.rows[partition] == 0 &&
			(!state->preserve_inner || spill->build.rows[partition] == 0))
		{
			side_release(&spill->build, partition);
			side_release(&spill->probe, partition);
			continue;
		}
		return open_partition(state, partition);
	}
	spill->partition = spill->npartitions;
	return false;
}

/*
 * The next batch of the joined partition's outer rows (or of a shared
 * table's outer rows it answers itself, spill->rows): up to
 * JOIN_COMPACT_ROWS records of the chunk being read, each stored column
 * gathered from their payload; the chunks come from the file, each after
 * its values, and then the tail. False when none is left.
 */
static bool
next_spilled(TessHashJoinState *state, JoinSpill *spill)
{
	SpillSide  *side = spill->rows;

	for (;;)
	{
		TessSpillHeader header;

		if (spill->block != NULL)
		{
			uint64		total = tess_spill_columns_rows(spill->block);

			if (spill->cursor < total)
			{
				int			count = (int) Min(total - spill->cursor, JOIN_COMPACT_ROWS);
				const uint64 *nulls = tess_spill_columns_lane(spill->block, 0) + spill->cursor;
				uint64		any = 0;

				for (int row = 0; row < count; row++)
					any |= nulls[row];
				spill->bits[0] = count == 64 ? ~UINT64CONST(0) :
					(UINT64CONST(1) << count) - 1;
				spill->batch.rows.nrows = count;
				spill->batch_ordinal = spill->ordinal;
				spill->ordinal += count;
				/* A by-value word is read where it lies; a reference becomes a pointer. */
				for (int word = 0; word < side->nwords; word++)
				{
					uint64	   *lane = tess_spill_columns_lane(spill->block, 1 + word) +
						spill->cursor;
					bool	   *isnull = spill->isnull[word];

					if ((any >> word) & 1)
						for (int row = 0; row < count; row++)
							isnull[row] = (nulls[row] >> word) & 1;
					else
						memset(isnull, 0, sizeof(bool) * count);
					if (side->byvals[word])
					{
						spill->window[word] = (Datum *) lane;
						continue;
					}
					spill->window[word] = spill->values[word];
					for (int row = 0; row < count; row++)
					{
						uint64		ref = lane[row];

						if (isnull[row])
						{
							spill->values[word][row] = (Datum) 0;
							continue;
						}
						if ((ref >> 32) - 1 >= (uint64) side->nvalues ||
							side->value_bases[(ref >> 32) - 1] == NULL)
							elog(ERROR, "TessHashJoin read back a value it did not keep");
						spill->values[word][row] =
							PointerGetDatum(side->value_bases[(ref >> 32) - 1] +
											(ref & 0xFFFFFFFF));
					}
				}
				spill->cursor += count;
				return true;
			}
			spill->block = NULL;
		}
		/* The next chunk: the previous one and its values go. */
		for (int index = 0; index < spill->nblock_values; index++)
			side->value_bases[spill->block_values[index]] = NULL;
		spill->nblock_values = 0;
		MemoryContextReset(spill->block_context);
		if (spill->reader.open)
		{
			while (part_header(&spill->reader, &header))
			{
				void	   *body = MemoryContextAllocExtended(spill->block_context,
															  Max(header.len, 8),
															  MCXT_ALLOC_HUGE);

				part_body(&spill->reader, body, header.len);
				if (header.kind == TESS_SPILL_VALUES)
				{
					/* Another writer's number may be past this one's. */
					if (header.number >= INT_MAX - 1)
						elog(ERROR, "TessHashJoin read back a value chunk it never wrote");
					side_value_slot(side, (int) header.number);
					side->nvalues = Max(side->nvalues, (int) header.number + 1);
					side->value_bases[header.number] = body;
					grow_ints(spill->context, &spill->block_values,
							  &spill->block_slots, spill->nblock_values + 1);
					spill->block_values[spill->nblock_values++] = header.number;
					continue;
				}
				spill->block = body;
				break;
			}
			if (spill->block == NULL)
				part_close(&spill->reader);
		}
		if (spill->block == NULL && !spill->tail_read)
		{
			int			index = side->current[spill->partition];

			spill->tail_read = true;
			if (index != 0 && side_chunk_rows(side, side->bases[index]))
			{
				spill->block = side->bases[index];
				state->counters[JOIN_TAILS]++;
			}
		}
		if (spill->block == NULL)
			return false;
		if (!side->columnar)
			elog(ERROR, "TessHashJoin reads back outer rows in chunks of columns only");
		spill->cursor = 0;
		note_memory(state);
	}
}

/* A column of a batch of outer rows read back: a stored column's values. */
static void
spill_get_column(TessBatch *batch, int column, const TessRowMask *rows,
				 TessColumnPurpose purpose, TessDatumColumn *result)
{
	JoinSpill  *spill = (JoinSpill *) batch->private_data;
	int			word;

	if (column < 0 || column >= spill->nchild || spill->word_of[column] == 0)
		elog(ERROR, "TessHashJoin did not keep outer column %d", column);
	word = spill->word_of[column] - 1;
	result->values = spill->window[word];
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
			if (spill->build.rows[partition] > 0)
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
	if (spill->bloom != NULL)
		check(state, state->kernels->bloom_probe(spill->bloom, spill->bloom_words,
												 state->hashes, &candidates, &pending,
												 &state->status));
	else
		memcpy(state->next_bits, state->pending_bits, sizeof(uint64) * nwords);
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

		if (at / 64 < spill->matched_words)
			word |= ((spill->matched_rows[at / 64] >> (at % 64)) & 1) << row;
	}
	return word;
}

/*
 * RIGHT and FULL: whether this participant returns the tail of the table
 * in memory now: a table of its own's, once; a shared table's, the last
 * participant to leave it, which the others leave here, their tail done.
 */
static bool
tail_turn(TessHashJoinState *state)
{
	if (!state->preserve_inner || state->table_tail_done)
		return false;
	if (state->marks_shared &&
		!(state->round_partition >= 0 ? round_depart(state) : leave_shared(state, true)))
	{
		state->table_tail_done = true;
		return false;
	}
	return true;
}

/*
 * RIGHT and FULL over a table that spills: before the table in memory goes
 * (the resident partitions', a piece's, a partition's), its records
 * without a pair go out; true asks the caller for that tail first.
 */
static bool
tail_first(TessHashJoinState *state)
{
	if (!tail_turn(state))
		return false;
	state->tail_request = true;
	return true;
}

/*
 * The next outer batch: the outer child's, while it has one, then those
 * of the partitions on disk, each joined in turn. The rows to answer are
 * the batch's selected ones, until the table sends some to disk.
 */
static TessBatch *
outer_next(TessHashJoinState *state)
{
	for (;;)
	{
		JoinSpill  *spill = state->spill;
		TessBatch  *batch = NULL;

		/* A shared table answers its rows first, then goes: not while a compact batch holds its pairs. */
		if (spill != NULL && spill->shared && !spill->resident_done)
		{
			batch = shared_resident_next(state);
			if (batch != NULL)
				return batch;
			/* RIGHT and FULL: the last one to leave the table returns its tail first. */
			if (state->holding || tail_first(state))
				return NULL;
			shared_resident_end(state);
			continue;
		}
		if (spill == NULL || !spill->joining)
		{
			/* The outer rows: the child's, or those of the partition a level splits. */
			if (spill == NULL || spill->parent == NULL)
			{
				if (spill == NULL || !spill->child_done)
					batch = tess_input_next(state->outer_input);
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
			}
			else if (!spill->child_done && next_spilled(state, spill->parent))
			{
				batch = &spill->parent->batch;
				reserve_rows(state, batch->rows.nrows);
				state->active_bits[0] = spill->parent->bits[0];
				return batch;
			}
			/* The resident table goes: not while a compact batch holds its pairs. */
			spill->child_done = true;
			if (state->holding || tail_first(state))
				return NULL;
			start_joining(state);
		}
		if (spill->partition >= 0 && spill->partition < spill->npartitions &&
			next_spilled(state, spill))
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
		/* A round's next outer file, probing the same table. */
		if (state->round_partition >= 0 && !state->round_departed &&
			round_next_outer(state))
			continue;
		/* RIGHT and FULL: the table's records without a pair before it goes. */
		if (state->holding || tail_first(state))
			return NULL;
		if (spill->partition >= 0 && spill->partition < spill->npartitions &&
			next_pass(state))
			continue;
		/* The next partition: loaded, or split into a level below. */
		if (spill->partition < spill->npartitions && next_partition(state))
			continue;
		/* The level is done: back to the level whose partition it split. */
		if (spill->parent == NULL)
			return NULL;
		pop_level(state);
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

	while (spill != NULL && batch != &spill->batch)
		spill = spill->parent;
	if (spill == NULL)
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
			/* A shared table's partition has outer rows of every participant, not counted. */
			if (at / 64 >= spill->matched_words)
			{
				Size		words = Max(spill->matched_words * 2, at / 64 + 1);

				spill->matched_rows = repalloc_huge(spill->matched_rows, sizeof(uint64) * words);
				memset(spill->matched_rows + spill->matched_words, 0,
					   sizeof(uint64) * (words - spill->matched_words));
				spill->matched_words = words;
			}
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
	while (spill != NULL)
	{
		JoinSpill  *parent = spill->parent;

		level_free(spill);
		spill = parent;
	}
	state->spill = NULL;
	state->holding = false;
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
 * inserted without grouping, and in a round's; a partition a participant
 * joins alone is a table of its own.
 */
static TessStatusCode
next_record(TessHashJoinState *state, const TessRowMask *rows, TessRowMask *found)
{
	if (state->chain_table)
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
 * RIGHT and FULL: the marks of a shared table of nchunks chunks, at
 * `marks` in the query's shared memory, which every participant sets.
 */
static void
share_marks(TessHashJoinState *state, dsa_pointer marks, int nchunks)
{
	uint64	   *words;

	forget_marks(state);
	if (!state->preserve_inner)
		return;
	if (!DsaPointerIsValid(marks))
		elog(ERROR, "TessHashJoin found no marks of its shared table");
	if (state->marks_context == NULL)
		state->marks_context = AllocSetContextCreate(state->css.ss.ps.state->es_query_cxt,
													 "TessHashJoin marks",
													 ALLOCSET_DEFAULT_SIZES);
	state->marks = MemoryContextAlloc(state->marks_context,
									  sizeof(uint64 *) * Max(nchunks, 1));
	words = dsa_get_address(query_dsa(state), marks);
	for (int chunk = 0; chunk < nchunks; chunk++)
		state->marks[chunk] = words + chunk * mark_words(state);
	state->mark_slots = nchunks;
	state->marks_shared = true;
}

/* The marks of a shared table of nchunks chunks, none set, for the elected participant. */
static dsa_pointer
allocate_marks(TessHashJoinState *state, int nchunks)
{
	if (!state->preserve_inner)
		return InvalidDsaPointer;
	return dsa_allocate_extended(query_dsa(state),
								 mul_size(mul_size(Max(nchunks, 1), mark_words(state)),
										  sizeof(uint64)),
								 DSA_ALLOC_ZERO | DSA_ALLOC_HUGE);
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
	/* A small hash_mem takes small chunks: they count against it before the table spills. */
	Size		len = state->nown == 0 ? JOIN_FIRST_CHUNK : chunk_len_for(JOIN_CHUNK_LEN);
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
	header->owner = state->spill_participant;
	state->own_base = (char *) header + JOIN_CHUNK_HEADER;
	state->own_len = len;
	check(state, state->kernels->table_chunk_init(state->own_base, len,
												  &state->status));
	header->next = *own_list(state, false);
	*own_list(state, false) = block;
	state->own_chunks[state->nown++] = (int) number;
	state->own_bytes += JOIN_CHUNK_HEADER + len;
	shared_count(state, record_chunk_cost(len, state->record_size));
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
		{
			note_prune_keys(state, batch);
			if (state->spill != NULL)
				insert_spill(state, batch);
			else
				insert_shared_batch(state, batch);
			shared_check(state);
		}
		tess_input_finish(state->inner_input);
	}
	share_prune_keys(state);
	check(state, state->kernels->build_report(state->shared->counters,
											  state->appended,
											  state->null_columns,
											  &state->status));
}

/*
 * SIZE: the index for exactly the records appended, the directory of the
 * chunks by number, and the filter, as the elected participant. A table
 * that spilled holds the records of the partitions in memory only, in
 * chunks no counter numbered: they are numbered here, in the lists'
 * order; the value chunks of the partitions on disk leave holes in the
 * values' directory.
 */
static void
size_shared_table(TessHashJoinState *state)
{
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	dsa_area   *area = query_dsa(state);
	TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
	bool		split = shared_partitions(state) > 0;
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
	if (split)
	{
		records = 0;
		for (uint32 partition = 0; partition < shared_partitions(state); partition++)
		{
			uint64		held;

			if (shared_on_disk(state, partition))
				continue;
			check(state, state->kernels->table_spill_records(shared_words(state),
															 state->shared->spill_nwords,
															 partition, 0, &held,
															 &state->status));
			records += held;
		}
		nchunks = 0;
		for (int participant = 0; participant < state->shared->participants; participant++)
			for (block = *participant_list(state, participant, false);
				 DsaPointerIsValid(block);)
			{
				JoinChunk  *header = dsa_get_address(area, block);

				header->number = nchunks++;
				block = header->next;
			}
		if (nchunks > TESS_TABLE_MAX_CHUNKS)
			ereport(ERROR,
					(errcode(ERRCODE_PROGRAM_LIMIT_EXCEEDED),
					 errmsg("TessHashJoin hash table cannot hold more than %d chunks",
							TESS_TABLE_MAX_CHUNKS)));
	}
	state->shared->resident_rows = records;
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
	for (int participant = 0; participant < state->shared->participants; participant++)
		for (block = *participant_list(state, participant, false);
			 DsaPointerIsValid(block);)
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
	state->shared->marks = allocate_marks(state, (int) nchunks);
	/* The value chunks' directory: dsa_pointers of their bases by number. */
	state->shared->nvalue_chunks = (int) state->shared->next_value_chunk;
	state->shared->value_directory =
		dsa_allocate_extended(area,
							  mul_size(Max(state->shared->nvalue_chunks, 1),
									   sizeof(dsa_pointer)),
							  DSA_ALLOC_ZERO | DSA_ALLOC_HUGE);
	bases = dsa_get_address(area, state->shared->value_directory);
	for (int participant = 0; participant < state->shared->participants; participant++)
		for (block = *participant_list(state, participant, true);
			 DsaPointerIsValid(block);)
		{
			JoinChunk  *header = dsa_get_address(area, block);

			if (header->number >= (uint64) state->shared->nvalue_chunks ||
				DsaPointerIsValid(bases[header->number]))
				elog(ERROR, "TessHashJoin found value chunk %llu out of the directory",
					 (unsigned long long) header->number);
			bases[header->number] = block + JOIN_CHUNK_HEADER;
			block = header->next;
		}
	for (int chunk = 0; chunk < state->shared->nvalue_chunks && !split; chunk++)
		if (!DsaPointerIsValid(bases[chunk]))
			elog(ERROR, "TessHashJoin is missing value chunk %d of its table", chunk);
	if (split)
		make_rounds(state);
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
	/* A table that spilled: this participant's chunks of the partitions in memory, as SIZE numbered them. */
	if (shared_partitions(state) > 0)
	{
		state->nown = 0;
		for (dsa_pointer block = *own_list(state, false); DsaPointerIsValid(block);)
		{
			JoinChunk  *header = dsa_get_address(query_dsa(state), block);

			if (state->nown == state->own_slots)
			{
				int			slots = Max(state->own_slots * 2, 8);

				state->own_chunks = state->own_chunks == NULL ?
					MemoryContextAlloc(state->css.ss.ps.state->es_query_cxt,
									   sizeof(int) * slots) :
					repalloc(state->own_chunks, sizeof(int) * slots);
				state->own_slots = slots;
			}
			state->own_chunks[state->nown++] = (int) header->number;
			block = header->next;
		}
	}
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
	for (int list = 0; list < 2 * state->shared->participants; list++)
	{
		dsa_pointer *head = participant_list(state, list / 2, list % 2 == 1);

		while (DsaPointerIsValid(*head))
		{
			dsa_pointer block = *head;

			*head = ((JoinChunk *) dsa_get_address(query_dsa(state), block))->next;
			dsa_free(query_dsa(state), block);
		}
	}
	if (DsaPointerIsValid(state->shared->marks))
	{
		dsa_free(query_dsa(state), state->shared->marks);
		state->shared->marks = InvalidDsaPointer;
	}
	if (DsaPointerIsValid(state->shared->spill_filter))
	{
		dsa_free(query_dsa(state), state->shared->spill_filter);
		state->shared->spill_filter = InvalidDsaPointer;
		state->shared->spill_filter_words = 0;
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
	state->spill_participant = IsParallelWorker() ? ParallelWorkerNumber + 1 : 0;
	state->round_partition = -1;
	reset_prune_keys(state);
	state->spill_words = NULL;
	state->spill_seen = 0;
	state->spill_over = false;
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
			case TESS_BUILD_DO_FLUSH:
				Assert(BarrierPhase(&state->shared->build) == TESS_BUILD_FLUSH);
				shared_flush(state);
				break;
			case TESS_BUILD_DO_OUTER:
				Assert(BarrierPhase(&state->shared->build) == TESS_BUILD_OUTER);
				shared_outer(state);
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
					share_marks(state, state->shared->marks, state->shared->nchunks);
					/* The whole table's rows and duplicates, every link done. */
					check(state, state->kernels->build_totals(state->shared->counters,
															  &records,
															  &state->null_columns,
															  &nchunks,
															  &state->duplicates,
															  &state->status));
					state->build_rows = records;
					state->chain_table = true;
					if (shared_partitions(state) > 0)
						shared_probe_start(state, records);
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

/*
 * Leave a shared build after probing; the last one to leave frees the
 * table, or with `keep`, RIGHT and FULL, owes the free until its tail is
 * done and the next call. True for the last one.
 */
static bool
leave_shared(TessHashJoinState *state, bool keep)
{
	uint32		reply = 0;

	if (state->table_free_owed)
	{
		state->table_free_owed = false;
		state->chain_table = false;
		free_shared_table(state);
		return false;
	}
	if (!state->participating)
		return false;
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
				state->participating = false;
				if (keep)
				{
					state->table_free_owed = true;
					return true;
				}
				free_shared_table(state);
				state->chain_table = false;
				return true;
			case TESS_BUILD_DONE:
				state->participating = false;
				state->chain_table = false;
				state->table.index = NULL;
				state->table.index_len = 0;
				state->table.nchunks = 0;
				return false;
			default:
				elog(ERROR, "TessHashJoin got build action %u out of order", action);
		}
	}
}

/*
 * A shared table that spills (docs/spill.md, "Shared tables"). The build
 * appends to chunks in the query's shared memory as ever, counting their
 * bytes in the words every participant decides by; the budget is every
 * participant's hash_mem, as the core's shared table has. The first
 * participant whose chunks pass it splits the table: every participant,
 * once it sees that, splits its own chunks so far into partitions whose
 * chunks are in shared memory too, and goes on appending partitioned.
 * While the chunks take more than the budget, the largest partition goes
 * to disk, and each participant writes its own chunks of it to its own
 * files. At FLUSH each writes its tails of the partitions on disk and
 * hands its chunks of the others to the table, which SIZE indexes and
 * LINK links. At OUTER every participant writes its share of the outer
 * side to files, before any row goes out, since a participant that
 * returns rows may not wait at a barrier: the rows of the partitions on
 * disk to theirs, those of the partitions in memory and, for a left or
 * anti join, those without a pair to a file the shared table answers. At
 * PROBE each reads such files, one at a time as it takes them, and
 * probes the shared table; then it leaves the build and takes partitions
 * on disk, each joined whole by the one that took it, from every
 * participant's files, as a serial table joins its partitions.
 */

/* Name a side's files in the table's set; the inner side's chunks go to shared memory. */
static void
side_share(TessHashJoinState *state, SpillSide *side, const char *prefix, bool memory)
{
	TessSpillConfig config = TESS_STRUCT_INITIALIZER(TessSpillConfig);

	tess_spill_free(side->file);
	config.parent_context = side->context;
	config.kernels = state->kernels;
	config.npartitions = side->npartitions;
	config.level = 0;
	config.fingerprint = side->fingerprint;
	config.max_len = (uint64) MaxAllocHugeSize;
	config.shared = &state->shared->fileset;
	config.participant = state->spill_participant;
	config.name = psprintf("%s%d", prefix, state->css.ss.ps.plan->plan_node_id);
	config.buffer_len = TESS_SPILL_BUFFER_LEN(get_hash_memory_limit());
	side->file = tess_spill_create(&config);
	side->shared_files = true;
	if (!memory)
		return;
	side->area = query_dsa(state);
	side->shared = state->shared;
	side->spill_words = shared_words(state);
	side->spill_nwords = state->shared->spill_nwords;
	side->kernels = state->kernels;
	side->owner = state->spill_participant;
}

/*
 * The first participant past the budget: the partitions, the power of two
 * that makes the inner side it expects (twice what the participants hold,
 * or the planner's estimate if more) about half of one participant's
 * hash_mem each, since a partition on disk is joined by one, and two per
 * participant at least; a filter of every inner row, published before
 * the split, so that whoever sees the split finds it.
 */
static void
shared_split(TessHashJoinState *state)
{
	JoinShared *shared = state->shared;
	dsa_area   *area = query_dsa(state);
	Size		limit = get_hash_memory_limit();
	int			participants = Max(shared->participants, 1);
	double		held = (double) state->own_bytes + state->value_bytes;
	double		expected = held * 2 * participants;
	uint64		rows = Max((uint64) state->inner_rows * participants,
						   state->build_rows * 2 * participants);
	uint32		npartitions = JOIN_SPILL_MIN_PARTITIONS;
	uint32		in_force;
	Size		nwords;
	dsa_pointer filter;

	if (state->build_rows > 0)
		expected = Max(expected, held / state->build_rows * state->inner_rows * participants);
	while (npartitions < JOIN_SPILL_MAX_PARTITIONS &&
		   (npartitions < 2 * (uint32) participants ||
			(double) npartitions * (limit / 2) < expected) &&
		   (Size) npartitions * 2 * (4 * JOIN_SPILL_MIN_CHUNK + 2 * BLCKSZ) <= limit / 2)
		npartitions *= 2;
	check(state, state->kernels->table_bloom_words(Max(rows, 1), &nwords, &state->status));
	filter = dsa_allocate_extended(area, mul_size(sizeof(uint64), nwords),
								   DSA_ALLOC_HUGE | DSA_ALLOC_ZERO);
	SpinLockAcquire(&shared->lock);
	if (!DsaPointerIsValid(shared->spill_filter))
	{
		shared->spill_filter = filter;
		shared->spill_filter_words = nwords;
		filter = InvalidDsaPointer;
	}
	SpinLockRelease(&shared->lock);
	if (DsaPointerIsValid(filter))
		dsa_free(area, filter);
	else
		state->counters[JOIN_BLOOM_FILTERS]++;
	check(state, state->kernels->table_spill_split(shared_words(state), shared->spill_nwords,
												   npartitions, &in_force,
												   &state->status));
}

/*
 * This participant sees the table split: a level of the partitions in
 * force, its inner side in shared memory and its files in the table's
 * set; its own chunks so far split into the partitions, their values
 * copied into the partitions' value chunks, then freed.
 */
static void
shared_switch(TessHashJoinState *state)
{
	JoinShared *shared = state->shared;
	dsa_area   *area = query_dsa(state);
	JoinSpill  *spill;
	dsa_pointer block;

	spill = spill_create(state, NULL, 0, 0, (int) shared_partitions(state));
	spill->shared = true;
	spill->writers = shared->participants;
	side_share(state, &spill->build, "tjb", true);
	side_share(state, &spill->probe, "tjo", false);
	side_share(state, &spill->resident, "tjr", false);
	/* The splitting participant published the filter before the split. */
	SpinLockAcquire(&shared->lock);
	block = shared->spill_filter;
	spill->bloom_words = shared->spill_filter_words;
	SpinLockRelease(&shared->lock);
	if (!DsaPointerIsValid(block))
		elog(ERROR, "TessHashJoin found its shared table split without a filter");
	spill->bloom = dsa_get_address(area, block);
	/* Only this participant takes from its lists before SIZE. */
	for (block = *own_list(state, false); DsaPointerIsValid(block);)
	{
		JoinChunk  *header = dsa_get_address(area, block);
		dsa_pointer next = header->next;

		split_chunk(state, (char *) header + JOIN_CHUNK_HEADER, header->len,
					state->value_bases);
		shared_count(state, -record_chunk_cost(header->len, state->record_size));
		dsa_free(area, block);
		block = next;
	}
	*own_list(state, false) = InvalidDsaPointer;
	for (block = *own_list(state, true); DsaPointerIsValid(block);)
	{
		JoinChunk  *header = dsa_get_address(area, block);
		dsa_pointer next = header->next;

		shared_count(state, -(int64) (JOIN_CHUNK_HEADER + header->len));
		dsa_free(area, block);
		block = next;
	}
	*own_list(state, true) = InvalidDsaPointer;
	/* The values of the chunks split are the partitions' now. */
	if (state->value_bases != NULL)
		memset(state->value_bases, 0, sizeof(char *) * state->value_slots);
	reset_values(state);
	state->nown = 0;
	state->own_base = NULL;
	state->own_len = 0;
	state->own_bytes = 0;
	spill->total_rows = state->build_rows;
	state->spill_seen = 0;
}

/*
 * Write this participant's chunks of the partitions others sent to disk;
 * then, when evict is set and the chunks pass the budget, send the
 * largest partition to disk and write this participant's chunks of it:
 * one per call, since the others write theirs of it at their next batch.
 */
static void
shared_sync(TessHashJoinState *state, bool evict)
{
	JoinSpill  *spill = state->spill;
	SpillSide  *side = &spill->build;
	uint64		evictions;
	int32		partition;

	check(state, state->kernels->table_spill_evictions(shared_words(state),
													   state->shared->spill_nwords,
													   &evictions, &state->status));
	if (evictions != state->spill_seen)
	{
		state->spill_seen = evictions;
		for (partition = 0; partition < spill->npartitions; partition++)
			if (side->parts[partition].resident && shared_on_disk(state, partition))
				side_demote(state, side, partition);
	}
	if (!evict)
		return;
	/* The count as of now, which the others' writes lowered. */
	side_count(side, -1, 0);
	if (!side->over)
		return;
	check(state, state->kernels->table_spill_evict(shared_words(state),
												   state->shared->spill_nwords,
												   &partition, &state->status));
	if (partition >= 0 && side->parts[partition].resident)
		side_demote(state, side, partition);
}

/* After a batch of the inner side: split, switch or send partitions to disk as needed. */
static void
shared_check(TessHashJoinState *state)
{
	if (state->spill == NULL)
	{
		if (state->spill_over && shared_partitions(state) == 0)
			shared_split(state);
		if (shared_partitions(state) > 0)
			shared_switch(state);
	}
	if (state->spill != NULL)
		shared_sync(state, true);
}

/* A participant of a table that spilled, one that joined late too: its level, as decided. */
static void
shared_join(TessHashJoinState *state)
{
	if (state->spill == NULL)
		shared_switch(state);
	shared_sync(state, false);
}

/*
 * FLUSH: this participant counts its records by partition, writes its
 * chunks of the partitions on disk, the tails too, and hands those of the
 * partitions in memory, with their values, to the table's lists.
 */
static void
shared_flush(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);
	JoinSpill  *spill;
	SpillSide  *side;

	if (shared_partitions(state) == 0)
		return;
	shared_join(state);
	spill = state->spill;
	side = &spill->build;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		SpillPart  *part = &side->parts[partition];

		if (side->rows[partition] > 0)
			check(state, state->kernels->table_spill_records(shared_words(state),
															 state->shared->spill_nwords,
															 partition, side->rows[partition],
															 NULL, &state->status));
		if (!part->resident)
		{
			pg_atomic_uint64 *stats = part_stats(state, partition);

			side_flush(state, side, partition);
			side_forget(side, partition);
			pg_atomic_fetch_add_u64(&stats[0], part->disk_bytes);
			pg_atomic_fetch_add_u64(&stats[1], part->blocks);
			continue;
		}
		for (int chunk = 0; chunk < part->nchunks; chunk++)
		{
			int			index = part->chunks[chunk];
			JoinChunk  *header = dsa_get_address(area, side->pointers[index]);

			header->next = *own_list(state, false);
			*own_list(state, false) = side->pointers[index];
			side->pointers[index] = InvalidDsaPointer;
			side->bases[index] = NULL;
		}
		for (int value = 0; value < part->nvalues; value++)
		{
			int			number = part->values[value];
			JoinChunk  *header = dsa_get_address(area, side->value_pointers[number]);

			header->next = *own_list(state, true);
			*own_list(state, true) = side->value_pointers[number];
			side->value_pointers[number] = InvalidDsaPointer;
			side->value_bases[number] = NULL;
		}
		side->bytes -= part->bytes;
		part->bytes = 0;
		part->nchunks = 0;
		part->nvalues = 0;
		part->value_current = -1;
		side->current[partition] = 0;
	}
	side_compact(side);
	tess_spill_finish(side->file);
}

/*
 * OUTER: this participant's share of the outer side, written whole: the
 * rows of the partitions on disk to theirs; those of the partitions in
 * memory, and for a left or anti join those the filter of every inner row
 * rejects or with a NULL key, which have no pair, to the rows the shared
 * table answers. Nothing goes out yet.
 */
static void
shared_outer(TessHashJoinState *state)
{
	bool		answer = state->jointype == JOIN_LEFT || state->jointype == JOIN_ANTI;
	JoinSpill  *spill;

	if (shared_partitions(state) == 0)
		return;
	shared_join(state);
	/* Past the build's barrier, the keys are every participant's. */
	prune_outer(state);
	spill = state->spill;
	for (;;)
	{
		TessBatch  *batch = tess_input_next(state->outer_input);
		int			nrows;
		int			nwords;
		TessRowMask valid;
		TessRowMask passed;
		TessRowMask resident;
		uint64		any_disk = 0;
		uint64		any_resident = 0;

		if (batch == NULL)
			break;
		nrows = batch->rows.nrows;
		nwords = tess_row_mask_word_count(nrows);
		state->counters[JOIN_PROBE_ROWS] += tess_row_mask_count(&batch->rows);
		reserve_rows(state, nrows);
		memset(state->valid_bits, 0, sizeof(uint64) * nwords);
		memset(state->next_bits, 0, sizeof(uint64) * nwords);
		valid = (TessRowMask) {nrows, state->valid_bits};
		passed = (TessRowMask) {nrows, state->next_bits};
		resident = (TessRowMask) {nrows, state->pending_bits};
		batch_keys(state, batch, state->outer_keys, state->outer_kinds, &valid);
		if (tess_row_mask_count(&valid) > 0)
			check(state, state->kernels->bloom_probe(spill->bloom, spill->bloom_words,
													 state->hashes, &valid, &passed,
													 &state->status));
		state->counters[JOIN_BLOOM_REMOVED] +=
			tess_row_mask_count(&valid) - tess_row_mask_count(&passed);
		/* The rows passed split into those on disk (next_bits) and in memory. */
		for (int word = 0; word < nwords; word++)
		{
			uint64		bits = state->next_bits[word];
			uint64		kept = 0;

			while (bits != 0)
			{
				int			bit = pg_rightmost_one_pos64(bits);
				int			partition = spill_partition(spill, state->hashes[word * 64 + bit]);

				bits &= bits - 1;
				if (spill->build.parts[partition].resident)
					kept |= UINT64CONST(1) << bit;
			}
			state->pending_bits[word] = kept |
				(answer ? batch->rows.bits[word] & ~state->next_bits[word] : 0);
			state->next_bits[word] &= ~kept;
			any_disk |= state->next_bits[word];
			any_resident |= state->pending_bits[word];
		}
		if (any_disk != 0)
			side_append(state, &spill->probe, batch, &passed, spill->stored, NULL);
		if (any_resident != 0)
			side_append(state, &spill->resident, batch, &resident, spill->stored, NULL);
		tess_input_finish(state->outer_input);
	}
	/* Every tail to disk: the others read them. */
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		side_flush(state, &spill->probe, partition);
		side_forget(&spill->probe, partition);
	}
	side_flush(state, &spill->resident, 0);
	side_forget(&spill->resident, 0);
	tess_spill_finish(spill->probe.file);
	tess_spill_finish(spill->resident.file);
}

/*
 * PROBE of a table that spilled: every participant's files are done; the
 * sizes of the partitions on disk are the table's, not this participant's.
 */
static void
shared_probe_start(TessHashJoinState *state, uint64 records)
{
	JoinSpill  *spill;
	Size		record;

	shared_join(state);
	spill = state->spill;
	record = TYPEALIGN(8, 16 + 8 * spill->build.nkeys + spill->build.payload_size);
	tess_spill_finish(spill->build.file);
	tess_spill_finish(spill->probe.file);
	tess_spill_finish(spill->resident.file);
	spill->joining = true;
	spill->partition = -1;
	spill->total_rows = records;
	spill->input_rows = records;
	check(state, state->kernels->table_spill_start(shared_words(state),
												   state->shared->spill_nwords,
												   (uint32 *) &spill->start, &state->status));
	spill->visited = 0;
	for (int partition = 0; partition < spill->npartitions; partition++)
	{
		uint64		rows;

		if (spill->build.parts[partition].resident)
		{
			state->counters[JOIN_RESIDENT]++;
			continue;
		}
		check(state, state->kernels->table_spill_records(shared_words(state),
														 state->shared->spill_nwords,
														 partition, 0, &rows,
														 &state->status));
		spill->build.rows[partition] = rows;
		spill->build.parts[partition].disk_bytes = rows * record;
	}
	state->build_rows = state->shared->resident_rows;
}

/*
 * The next batch of the outer rows the shared table answers: from a file
 * this participant took, the next one once it is done. NULL when every
 * file is taken.
 */
static TessBatch *
shared_resident_next(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	for (;;)
	{
		uint32		writer;

		if (spill->rows == &spill->resident && next_spilled(state, spill))
		{
			reserve_rows(state, spill->batch.rows.nrows);
			state->active_bits[0] = spill->bits[0];
			return &spill->batch;
		}
		check(state, state->kernels->table_spill_take_file(shared_words(state),
														   state->shared->spill_nwords,
														   spill->npartitions, true,
														   &writer, &state->status));
		if (writer >= (uint32) spill->writers)
			return NULL;
		spill->rows = &spill->resident;
		part_open(&spill->reader, spill->resident.file, 0, (int) writer + 1);
		spill->reader.next = (int) writer;
		spill->partition = 0;
		spill->tail_read = true;
		spill->block = NULL;
		spill->ordinal = 0;
	}
}

/*
 * The shared table answered its rows: this participant leaves it, the
 * last one freeing it, and joins the partitions on disk it takes.
 */
static void
shared_resident_end(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	for (int index = 0; index < spill->nblock_values; index++)
		spill->resident.value_bases[spill->block_values[index]] = NULL;
	spill->nblock_values = 0;
	MemoryContextReset(spill->block_context);
	spill->block = NULL;
	spill->rows = &spill->probe;
	spill->partition = -1;
	spill->resident_done = true;
	/* The filter goes with the table. */
	spill->bloom = NULL;
	spill->bloom_words = 0;
	leave_shared(state, false);
	forget_marks(state);
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->bloom = NULL;
	state->bloom_words = 0;
	state->build_rows = 0;
	state->duplicates = 0;
}

/* Whether any participant wrote outer rows of a partition. */
static bool
shared_has_outer(JoinSpill *spill, int partition)
{
	for (int writer = 0; writer < spill->writers; writer++)
	{
		TessSpillReader *reader = tess_spill_open(spill->probe.file, writer, partition);
		TessSpillHeader header;
		bool		found;

		if (reader == NULL)
			continue;
		found = tess_spill_read_header(reader, &header);
		tess_spill_close(reader);
		if (found)
			return true;
	}
	return false;
}

/* The two counters of a partition of a shared table: bytes on disk and blocks of records. */
static pg_atomic_uint64 *
part_stats(TessHashJoinState *state, int partition)
{
	return (pg_atomic_uint64 *) dsa_get_address(query_dsa(state), state->shared->part_stats) +
		2 * partition;
}

static JoinRound *
round_of(TessHashJoinState *state, int partition)
{
	Assert(partition >= 0 && partition < state->shared->nrounds);
	return (JoinRound *) dsa_get_address(query_dsa(state), state->shared->rounds) + partition;
}

/* Free what a round holds in shared memory. */
static void
round_release(dsa_area *area, JoinRound *round)
{
	if (DsaPointerIsValid(round->directory))
	{
		dsa_pointer *chunks = dsa_get_address(area, round->directory);

		for (int chunk = 0; chunk < round->nchunks; chunk++)
			if (DsaPointerIsValid(chunks[chunk]))
				dsa_free(area, chunks[chunk]);
		dsa_free(area, round->directory);
		round->directory = InvalidDsaPointer;
	}
	if (DsaPointerIsValid(round->values))
	{
		dsa_pointer *values = dsa_get_address(area, round->values);

		for (int number = 0; number < round->nvalues; number++)
			if (DsaPointerIsValid(values[number]))
				dsa_free(area, values[number]);
		dsa_free(area, round->values);
		round->values = InvalidDsaPointer;
	}
	if (DsaPointerIsValid(round->index))
	{
		dsa_free(area, round->index);
		round->index = InvalidDsaPointer;
	}
	if (DsaPointerIsValid(round->marks))
	{
		dsa_free(area, round->marks);
		round->marks = InvalidDsaPointer;
	}
}

/*
 * SIZE of a table that spilled: a round per partition. One on disk whose
 * blocks and index fit in one participant's hash_mem is loaded by every
 * participant together; a larger one is joined by one alone, which may
 * split it or take it in pieces.
 */
static void
make_rounds(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);
	Size		limit = get_hash_memory_limit();
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	int			npartitions = (int) shared_partitions(state);
	JoinRound  *rounds;

	Assert(!DsaPointerIsValid(state->shared->rounds));
	state->shared->rounds = dsa_allocate_extended(area, mul_size(npartitions, sizeof(JoinRound)),
												  DSA_ALLOC_ZERO);
	state->shared->nrounds = npartitions;
	rounds = dsa_get_address(area, state->shared->rounds);
	for (int partition = 0; partition < npartitions; partition++)
	{
		JoinRound  *round = &rounds[partition];
		pg_atomic_uint64 *stats = part_stats(state, partition);
		uint64		bytes = pg_atomic_read_u64(&stats[0]);
		uint64		blocks = pg_atomic_read_u64(&stats[1]);
		Size		size;

		BarrierInit(&round->barrier, 0);
		pg_atomic_init_u32(&round->next_chunk, 0);
		round->index = InvalidDsaPointer;
		round->directory = InvalidDsaPointer;
		round->values = InvalidDsaPointer;
		if (!shared_on_disk(state, partition))
			continue;
		check(state, state->kernels->table_spill_records(shared_words(state),
														 state->shared->spill_nwords,
														 partition, 0, &round->records,
														 &state->status));
		check(state, state->kernels->table_size(state->nkeys, state->inner_kinds, payload_size,
												Max(round->records, JOIN_INITIAL_ROWS),
												&size, &state->status));
		round->nchunks = (int) Min(blocks, (uint64) INT_MAX);
		round->nvalues = (int) state->shared->next_value_chunk;
		round->together = blocks > 0 && blocks <= TESS_TABLE_MAX_CHUNKS &&
			(double) bytes + size <= (double) limit;
	}
}

/* Free every round's memory and the rounds, at a rescan. */
static void
free_rounds(TessHashJoinState *state)
{
	dsa_area   *area = query_dsa(state);

	if (!DsaPointerIsValid(state->shared->rounds))
		return;
	for (int partition = 0; partition < state->shared->nrounds; partition++)
		round_release(area, round_of(state, partition));
	dsa_free(area, state->shared->rounds);
	state->shared->rounds = InvalidDsaPointer;
	state->shared->nrounds = 0;
}

/* ALLOCATE, as the round's elected one: the index for its records, and the directories. */
static void
round_allocate(TessHashJoinState *state, JoinRound *round)
{
	dsa_area   *area = query_dsa(state);
	Size		payload_size = sizeof(uint64) * (1 + state->npayload);
	uint64		capacity = Max(round->records, JOIN_INITIAL_ROWS);
	Size		size;

	check(state, state->kernels->table_size(state->nkeys, state->inner_kinds, payload_size,
											capacity, &size, &state->status));
	round->index = dsa_allocate_extended(area, size, DSA_ALLOC_HUGE);
	round->index_len = size;
	check(state, state->kernels->table_create(dsa_get_address(area, round->index), size,
											  state->nkeys, state->inner_kinds, payload_size,
											  capacity, &state->status));
	round->directory = dsa_allocate_extended(area,
											 mul_size(Max(round->nchunks, 1),
													  sizeof(dsa_pointer) + sizeof(Size)),
											 DSA_ALLOC_ZERO | DSA_ALLOC_HUGE);
	round->values = dsa_allocate_extended(area,
										  mul_size(Max(round->nvalues, 1), sizeof(dsa_pointer)),
										  DSA_ALLOC_ZERO | DSA_ALLOC_HUGE);
	round->marks = allocate_marks(state, round->nchunks);
	state->counters[JOIN_ROUNDS]++;
}

/*
 * LOAD: the inner files this participant takes, block by block into
 * shared memory; a chunk of records takes the next number of the round's
 * directory and is linked at once. A link reads only its own chunk and
 * the buckets, so the chunks others load stand as empty ones here.
 */
static void
round_load(TessHashJoinState *state, JoinSpill *spill, int partition, JoinRound *round)
{
	dsa_area   *area = query_dsa(state);
	dsa_pointer *chunks = dsa_get_address(area, round->directory);
	Size	   *lens = (Size *) (chunks + Max(round->nchunks, 1));
	dsa_pointer *values = dsa_get_address(area, round->values);
	int			nchunks = Max(round->nchunks, 1);
	void	   *empty = MemoryContextAlloc(spill->part_context, TESS_TABLE_CHUNK_HEADER);
	TessTableRef ref;

	check(state, state->kernels->table_chunk_init(empty, TESS_TABLE_CHUNK_HEADER,
												  &state->status));
	spill->round_bases = MemoryContextAlloc(spill->part_context, sizeof(void *) * nchunks);
	spill->round_lens = MemoryContextAlloc(spill->part_context, sizeof(Size) * nchunks);
	for (int chunk = 0; chunk < nchunks; chunk++)
	{
		spill->round_bases[chunk] = empty;
		spill->round_lens[chunk] = TESS_TABLE_CHUNK_HEADER;
	}
	ref.index = dsa_get_address(area, round->index);
	ref.index_len = round->index_len;
	ref.chunks = spill->round_bases;
	ref.chunk_lens = spill->round_lens;
	ref.nchunks = round->nchunks;
	for (;;)
	{
		TessSpillReader *reader;
		TessSpillHeader header;
		uint32		writer;

		check(state, state->kernels->table_spill_take_file(shared_words(state),
														   state->shared->spill_nwords,
														   partition, false, &writer,
														   &state->status));
		if (writer >= (uint32) spill->writers)
			break;
		reader = tess_spill_open(spill->build.file, (int) writer, partition);
		if (reader == NULL)
			continue;
		while (tess_spill_read_header(reader, &header))
		{
			dsa_pointer block = dsa_allocate_extended(area, Max(header.len, 8), DSA_ALLOC_HUGE);
			char	   *body = dsa_get_address(area, block);
			uint32		slot;
			Size		from = TESS_TABLE_CHUNK_HEADER;

			tess_spill_read_body(reader, body, header.len);
			if (header.kind == TESS_SPILL_VALUES)
			{
				if (header.number >= (uint32) round->nvalues ||
					DsaPointerIsValid(values[header.number]))
					elog(ERROR, "TessHashJoin read value chunk %u of a round twice or past its values",
						 header.number);
				values[header.number] = block;
				continue;
			}
			slot = pg_atomic_fetch_add_u32(&round->next_chunk, 1);
			if (slot >= (uint32) round->nchunks)
				elog(ERROR, "TessHashJoin read more chunks of a round than were written");
			chunks[slot] = block;
			lens[slot] = header.len;
			spill->round_bases[slot] = body;
			spill->round_lens[slot] = header.len;
			check(state, state->kernels->table_link(&ref, (int) slot, &from, NULL, NULL,
													&state->status));
		}
		tess_spill_close(reader);
	}
}

/* PROBE: the round's table as every participant loaded it. */
static void
round_attach(TessHashJoinState *state, JoinSpill *spill, JoinRound *round)
{
	dsa_area   *area = query_dsa(state);
	dsa_pointer *chunks = dsa_get_address(area, round->directory);
	Size	   *lens = (Size *) (chunks + Max(round->nchunks, 1));
	dsa_pointer *values = dsa_get_address(area, round->values);

	if (pg_atomic_read_u32(&round->next_chunk) != (uint32) round->nchunks)
		elog(ERROR, "TessHashJoin loaded %u chunks of a round of %d",
			 pg_atomic_read_u32(&round->next_chunk), round->nchunks);
	reserve_chunks(state, Max(round->nchunks, 1));
	for (int chunk = 0; chunk < round->nchunks; chunk++)
	{
		state->chunk_bases[chunk] = dsa_get_address(area, chunks[chunk]);
		state->chunk_lens[chunk] = lens[chunk];
	}
	state->table.index = dsa_get_address(area, round->index);
	state->table.index_len = round->index_len;
	state->table.nchunks = round->nchunks;
	spill->round_values = MemoryContextAllocZero(spill->part_context,
												 sizeof(char *) * Max(round->nvalues, 1));
	for (int number = 0; number < round->nvalues; number++)
		if (DsaPointerIsValid(values[number]))
			spill->round_values[number] = dsa_get_address(area, values[number]);
	state->value_bases = spill->round_values;
	state->nvalue_chunks = round->nvalues;
	state->build_rows = round->records;
	/* The links counted none: the chains are walked for more. */
	state->duplicates = state->inner_unique ? 0 : 1;
	state->bloom = NULL;
	state->bloom_words = 0;
	state->bloom_decided = true;
	state->chain_table = true;
	share_marks(state, round->marks, round->nchunks);
	note_memory(state);
}

/*
 * Take part in the round over a partition: true once this participant
 * probes it, false when the round is past loading, its outer rows all
 * taken.
 */
static bool
round_join(TessHashJoinState *state, int partition)
{
	JoinRound  *round = round_of(state, partition);
	uint32		reply = 0;

	memset(&state->round_participant, 0, sizeof(state->round_participant));
	for (;;)
	{
		uint32		action;

		check(state, state->kernels->round_step(&state->round_participant, reply, &action,
												&state->status));
		reply = 0;
		switch (action)
		{
			case TESS_BUILD_ATTACH:
				reply = BarrierAttach(&round->barrier);
				break;
			case TESS_BUILD_ARRIVE_AND_WAIT:
				reply = BarrierArriveAndWait(&round->barrier, PG_WAIT_EXTENSION) ? 1 : 0;
				break;
			case TESS_BUILD_DO_ALLOCATE:
				Assert(BarrierPhase(&round->barrier) == TESS_ROUND_ALLOCATE);
				round_allocate(state, round);
				break;
			case TESS_BUILD_DO_LOAD:
				Assert(BarrierPhase(&round->barrier) == TESS_ROUND_LOAD);
				round_load(state, state->spill, partition, round);
				break;
			case TESS_BUILD_DO_PROBE:
				Assert(BarrierPhase(&round->barrier) == TESS_ROUND_PROBE);
				round_attach(state, state->spill, round);
				state->round_partition = partition;
				return true;
			case TESS_BUILD_DETACH:
				BarrierDetach(&round->barrier);
				break;
			case TESS_BUILD_DONE:
				return false;
			default:
				elog(ERROR, "TessHashJoin got round action %u out of order", action);
		}
	}
}

/*
 * Leave the round probed, without waiting: true for the last one, which
 * frees it; RIGHT and FULL leave before the tail, which the last one
 * returns first, owing the free.
 */
static bool
round_depart(TessHashJoinState *state)
{
	JoinRound  *round = round_of(state, state->round_partition);
	uint32		reply = 0;

	Assert(!state->round_departed);
	state->round_departed = true;
	for (;;)
	{
		uint32		action;

		check(state, state->kernels->round_step(&state->round_participant, reply, &action,
												&state->status));
		reply = 0;
		switch (action)
		{
			case TESS_BUILD_ARRIVE_AND_DETACH:
				reply = BarrierArriveAndDetach(&round->barrier) ? 1 : 0;
				break;
			case TESS_BUILD_DO_FREE:
				state->round_free_owed = true;
				return true;
			case TESS_BUILD_DONE:
				return false;
			default:
				elog(ERROR, "TessHashJoin got round action %u out of order", action);
		}
	}
}

/* Leave the round probed, unless left already; the last one frees it. */
static void
round_leave(TessHashJoinState *state)
{
	JoinRound  *round = round_of(state, state->round_partition);

	if (!state->round_departed)
		(void) round_depart(state);
	if (state->round_free_owed)
		round_release(query_dsa(state), round);
	state->round_departed = false;
	state->round_free_owed = false;
	state->round_partition = -1;
	state->chain_table = false;
	state->table.index = NULL;
	state->table.nchunks = 0;
	state->value_bases = NULL;
	state->nvalue_chunks = 0;
}

/* The next outer file of the round's partition this participant takes; false when none is left. */
static bool
round_next_outer(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;
	uint32		writer;

	check(state, state->kernels->table_spill_take_file(shared_words(state),
													   state->shared->spill_nwords,
													   spill->partition, true, &writer,
													   &state->status));
	if (writer >= (uint32) spill->writers)
		return false;
	part_open(&spill->reader, spill->probe.file, spill->partition, (int) writer + 1);
	spill->reader.next = (int) writer;
	spill->tail_read = true;
	spill->block = NULL;
	spill->ordinal = 0;
	return true;
}

static bool open_partition(TessHashJoinState *state, int partition);

/*
 * The next partition on disk of a shared table, from this participant's
 * start round the circle: a round it takes part in, or one it joins alone.
 * False when it visited every one.
 */
static bool
shared_next_partition(TessHashJoinState *state)
{
	JoinSpill  *spill = state->spill;

	while (spill->visited < spill->npartitions)
	{
		int			partition = (spill->start + spill->visited++) % spill->npartitions;
		bool		taken;

		if (spill->build.parts[partition].resident)
			continue;
		/* RIGHT and FULL return a partition's inner rows without outer ones too. */
		if ((!state->preserve_inner || spill->build.rows[partition] == 0) &&
			!shared_has_outer(spill, partition))
			continue;
		if (round_of(state, partition)->together)
		{
			if (!round_join(state, partition))
				continue;
			/* The outer files come one at a time, as next_pass takes them. */
			spill->partition = partition;
			spill->reader.open = false;
			spill->tail_read = true;
			spill->block = NULL;
			return true;
		}
		check(state, state->kernels->table_spill_take_alone(shared_words(state),
															state->shared->spill_nwords,
															partition, &taken,
															&state->status));
		if (!taken)
			continue;
		state->counters[JOIN_ALONE]++;
		spill->probe.rows[partition] = 1;
		return open_partition(state, partition);
	}
	spill->partition = spill->npartitions;
	return false;
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
	/* The tail: inner rows without a pair, their outer columns NULL. */
	if (state->sides[column] == JOIN_SIDE_OUTER && state->tail)
	{
		result->values = state->null_values;
		result->isnull = state->null_isnull;
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

	/*
	 * A table that spills: the filter knows only the resident rows. A
	 * hashed key: the scan below has the value, not its hash.
	 */
	if ((state->jointype != JOIN_INNER && state->jointype != JOIN_SEMI) ||
		state->spill != NULL || state->hashed_keys)
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
			state->holding = count > 0;
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
 * RIGHT and FULL: mark the records of the published pairs, which passed
 * the join clauses. A reference is a chunk's number and a place in 8-byte
 * units (tessera/table.h); a chunk's records follow its header, each of
 * record_size bytes.
 */
static void
mark_pairs(TessHashJoinState *state)
{
	const TessRowMask *rows = &state->batch.rows;
	int			nwords = tess_row_mask_word_count(rows->nrows);

	if (state->marks == NULL || state->mark_slots < state->table.nchunks)
	{
		int			slots = Max(state->table.nchunks, 16);
		uint64	  **marks;

		if (state->marks_context == NULL)
			state->marks_context = AllocSetContextCreate(state->css.ss.ps.state->es_query_cxt,
														 "TessHashJoin marks",
														 ALLOCSET_DEFAULT_SIZES);
		marks = MemoryContextAllocZero(state->marks_context, sizeof(uint64 *) * slots);

		if (state->marks != NULL)
			memcpy(marks, state->marks, sizeof(uint64 *) * state->mark_slots);
		state->marks = marks;
		state->mark_slots = slots;
	}
	for (int word = 0; word < nwords; word++)
		for (uint64 bits = rows->bits[word]; bits != 0; bits &= bits - 1)
		{
			uint32		ref = state->current_offsets[word * 64 +
												   pg_rightmost_one_pos64(bits)];
			int			chunk = (int) (ref >> TESS_TABLE_UNIT_BITS);
			Size		byte = (Size) (ref & ((1u << TESS_TABLE_UNIT_BITS) - 1)) * 8;
			Size		index = (byte - TESS_TABLE_CHUNK_HEADER) / state->record_size;
			uint64		bit = UINT64CONST(1) << (index % 64);

			/* A shared table's: other participants set bits of the same words. */
			if (state->marks_shared)
			{
				pg_atomic_uint64 *word = (pg_atomic_uint64 *) &state->marks[chunk][index / 64];

				if ((pg_atomic_read_u64(word) & bit) == 0)
					(void) pg_atomic_fetch_or_u64(word, bit);
				continue;
			}
			if (state->marks[chunk] == NULL)
				state->marks[chunk] =
					MemoryContextAllocZero(state->marks_context,
										   sizeof(uint64) *
										   ((state->table.chunk_lens[chunk] /
											 state->record_size + 63) / 64));
			state->marks[chunk][index / 64] |= bit;
		}
}

/* Start the tail: the inner rows without a pair, from the first chunk on. */
static void
start_tail(TessHashJoinState *state)
{
	reserve_rows(state, JOIN_COMPACT_ROWS);
	state->tail = true;
	state->tail_chunk = 0;
	state->tail_byte = TESS_TABLE_CHUNK_HEADER;
	state->output_compact = false;
	state->null_round = false;
}

/*
 * The next records without a pair, up to a batch of them, published with
 * NULL outer columns: a chunk's used mark is its first word. False when
 * the walk is over.
 */
static bool
next_tail(TessHashJoinState *state)
{
	int			count = 0;

	while (count < JOIN_COMPACT_ROWS && state->tail_chunk < state->table.nchunks)
	{
		int			chunk = state->tail_chunk;
		uint64		used = *(const uint64 *) state->table.chunks[chunk];
		Size		index;

		if (state->tail_byte >= used)
		{
			state->tail_chunk++;
			state->tail_byte = TESS_TABLE_CHUNK_HEADER;
			continue;
		}
		index = (state->tail_byte - TESS_TABLE_CHUNK_HEADER) / state->record_size;
		if (state->marks == NULL || chunk >= state->mark_slots ||
			state->marks[chunk] == NULL ||
			((state->marks[chunk][index / 64] >> (index % 64)) & 1) == 0)
			state->tail_refs[count++] = ((uint32) chunk << TESS_TABLE_UNIT_BITS) |
				(uint32) (state->tail_byte / 8);
		state->tail_byte += state->record_size;
	}
	if (count == 0)
		return false;
	state->tail_bits[0] = count == 64 ? ~UINT64CONST(0) :
		(UINT64CONST(1) << count) - 1;
	state->batch.rows.nrows = count;
	state->batch.rows.bits = state->tail_bits;
	state->current_offsets = state->tail_refs;
	state->current_bits = state->tail_bits;
	state->nulls_gathered = false;
	memset(state->gathered, 0, sizeof(bool) * Max(state->npayload, 1));
	return true;
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
		if (state->tail)
		{
			if (!next_tail(state))
			{
				/*
				 * The table's tail is done: the join goes on past the table
				 * that asked for it, or is over.
				 */
				state->tail = false;
				state->table_tail_done = true;
				if (!state->tail_request)
					return false;
				state->tail_request = false;
				continue;
			}
		}
		else if (state->compact ? !fill_compact(state) : !next_round(state))
		{
			/* RIGHT and FULL: then the inner rows without a pair, unless asked for already. */
			if (!state->tail_request && !tail_turn(state))
				return false;
			start_tail(state);
			continue;
		}
		/* The join clauses decide the pairs; the rows without one have none. */
		if (state->qual != NULL && !state->null_round && !state->tail)
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
		if (state->preserve_inner && !state->null_round && !state->tail)
			mark_pairs(state);
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
	/*
	 * The residual clauses read their columns of the pairs too, and an outer
	 * join's filters theirs of the rows it returns, a column no parent may
	 * ask for, as in WHERE inner.c IS NULL above a left join.
	 */
	if (state->qual != NULL)
		needed = bms_add_members(needed, tess_qual_columns(state->qual));
	if (state->filter != NULL)
		needed = bms_add_members(needed, tess_qual_columns(state->filter));
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
	/* A record: its header, a slot per key and the payload's words. */
	state->record_size = 16 + sizeof(uint64) * (state->nkeys + 1 + state->npayload);
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
	List	   *hashers = tess_plan_read_int_list(reader, "key_hashers");
	List	   *collations = tess_plan_read_int_list(reader, "key_collations");
	List	   *prune_params;
	ListCell   *side;
	ListCell   *column;
	int			index = 0;

	state->residual_batch = tess_plan_read_int_list(reader, "residual_batch");
	state->filter_batch = tess_plan_read_int_list(reader, "filter_batch");
	state->jointype = (JoinType) tess_plan_read_int(reader, "jointype");
	state->plan_jointype = state->jointype;
	/* RIGHT is INNER and FULL is LEFT, the unmatched inner rows added last. */
	if (state->jointype == JOIN_RIGHT || state->jointype == JOIN_FULL)
	{
		state->preserve_inner = true;
		state->jointype = state->jointype == JOIN_RIGHT ? JOIN_INNER : JOIN_LEFT;
	}
	state->inner_unique = tess_plan_read_int(reader, "inner_unique") != 0;
	state->inner_rows = tess_plan_read_int(reader, "inner_rows");
	state->shared_mode = tess_plan_read_int(reader, "shared") != 0;
	state->prune_key = tess_plan_read_int(reader, "prune_key");
	prune_params = tess_plan_read_int_list(reader, "prune_params");
	state->prune_values = (PartitionPruneInfo *) tess_plan_read_node(reader, "prune_values");
	state->prune_range = (PartitionPruneInfo *) tess_plan_read_node(reader, "prune_range");
	state->round_partition = -1;
	tess_plan_reader_finish(reader);
	if (state->prune_key >= 0)
	{
		if (list_length(prune_params) != 3 || state->prune_values == NULL ||
			!IsA(state->prune_values, PartitionPruneInfo) ||
			(state->prune_range != NULL && !IsA(state->prune_range, PartitionPruneInfo)))
			elog(ERROR, "TessHashJoin received foreign plan data");
		for (int param = 0; param < 3; param++)
			state->prune_params[param] = list_nth_int(prune_params, param);
	}
	state->nkeys = list_length(outer_keys);
	if (list_length(sides) != state->ncolumns ||
		list_length(columns) != state->ncolumns ||
		state->nkeys < 1 || state->nkeys > TESS_TABLE_MAX_KEYS ||
		list_length(inner_keys) != state->nkeys ||
		list_length(outer_kinds) != state->nkeys ||
		list_length(inner_kinds) != state->nkeys ||
		list_length(hashers) != state->nkeys ||
		list_length(collations) != state->nkeys ||
		(state->jointype != JOIN_INNER && state->jointype != JOIN_SEMI &&
		 state->jointype != JOIN_ANTI && state->jointype != JOIN_LEFT) ||
		(state->filter_batch != NIL &&
		 state->jointype != JOIN_LEFT && state->jointype != JOIN_ANTI &&
		 !state->preserve_inner))
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
		state->collations[key] = (Oid) list_nth_int(collations, key);
		state->hashers[key].fn_oid = InvalidOid;
		if (OidIsValid((Oid) list_nth_int(hashers, key)))
		{
			if (state->outer_kinds[key] != TESS_TABLE_KEY_INT8 ||
				state->inner_kinds[key] != TESS_TABLE_KEY_INT8)
				elog(ERROR, "TessHashJoin received foreign plan data");
			fmgr_info((Oid) list_nth_int(hashers, key), &state->hashers[key]);
			state->hashed_keys = true;
		}
	}
	if (state->prune_key >= state->nkeys ||
		(state->prune_key >= 0 && OidIsValid(state->hashers[state->prune_key].fn_oid)))
		elog(ERROR, "TessHashJoin received foreign plan data");
	if (state->hashed_keys)
		state->hash_context = AllocSetContextCreate(CurrentMemoryContext,
													"TessHashJoin key hashes",
													ALLOCSET_DEFAULT_SIZES);
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
	if (state->prune_key >= 0 &&
		tess_append_join_prune_begin(state->outer, state->prune_values, state->prune_range,
									 state->prune_params))
	{
		state->prune_on = true;
		state->prune_keys.int8 = state->inner_kinds[state->prune_key] == TESS_TABLE_KEY_INT8;
		state->prune_keys.values = palloc_array(int64, JOIN_PRUNE_VALUES);
	}
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
	prune_outer(state);
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
	/* RIGHT and FULL: a table kept for the next scan has no pair yet. */
	state->tail = false;
	state->tail_request = false;
	state->table_tail_done = false;
	/* A shared table's go with it: the build starts anew. */
	if (state->marks_shared)
		forget_marks(state);
	for (int chunk = 0; state->marks != NULL && chunk < state->mark_slots; chunk++)
		if (state->marks[chunk] != NULL)
			memset(state->marks[chunk], 0,
				   sizeof(uint64) * ((state->table.chunk_lens[chunk] /
									  state->record_size + 63) / 64));
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
	if (state->round_partition >= 0)
		round_leave(state);
	if (state->shared != NULL)
		leave_shared(state, false);
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
	/* A shared table's participants share a budget: EXPLAIN compares the total. */
	values[JOIN_OVERRUN] = state->shared_budget == 0 && state->peak_memory > limit ?
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
	uint64		overrun;

	context = set_deparse_context_plan(es->deparse_cxt, css->ss.ps.plan,
									   ancestors);
	if (state->plan_jointype != JOIN_INNER)
		ExplainPropertyText("Join Type",
							state->plan_jointype == JOIN_SEMI ? "Semi" :
							state->plan_jointype == JOIN_ANTI ? "Anti" :
							state->plan_jointype == JOIN_RIGHT ? "Right" :
							state->plan_jointype == JOIN_FULL ? "Full" : "Left", es);
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
	if (state->shared_budget > 0)
		overrun = totals[JOIN_MEMORY] > state->shared_budget ?
			totals[JOIN_MEMORY] - state->shared_budget : 0;
	else
		overrun = totals[JOIN_OVERRUN];
	if (overrun > 0)
		ExplainPropertyInteger("Overrun", "kB", (overrun + 1023) / 1024, es);
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
		if (totals[JOIN_SPLITS] > 0)
			ExplainPropertyInteger("Split Partitions", NULL, totals[JOIN_SPLITS], es);
		if (totals[JOIN_PASSES] > 0)
			ExplainPropertyInteger("Extra Passes", NULL, totals[JOIN_PASSES], es);
		if (totals[JOIN_ROUNDS] > 0)
			ExplainPropertyInteger("Partitions Joined Together", NULL, totals[JOIN_ROUNDS], es);
		if (totals[JOIN_ALONE] > 0)
			ExplainPropertyInteger("Partitions Joined Alone", NULL, totals[JOIN_ALONE], es);
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
init_shared(TessHashJoinState *state, int participants, dsm_segment *segment)
{
	dsa_area   *area = query_dsa(state);
	Size		budget = get_hash_memory_limit();

	BarrierInit(&state->shared->build, 0);
	state->shared->index = InvalidDsaPointer;
	state->shared->index_len = 0;
	state->shared->directory = InvalidDsaPointer;
	state->shared->nchunks = 0;
	state->shared->marks = InvalidDsaPointer;
	state->shared->filter = InvalidDsaPointer;
	state->shared->filter_words = 0;
	SpinLockInit(&state->shared->lock);
	state->shared->next_value_chunk = 0;
	state->shared->value_directory = InvalidDsaPointer;
	state->shared->nvalue_chunks = 0;
	check(state, state->kernels->build_counters_init(state->shared->counters,
													 &state->status));
	/*
	 * Spilling: the words for the most partitions, and the files; the
	 * budget is every participant's hash_mem, as the core's shared table
	 * has. A rescan keeps the files' set and deletes the files.
	 */
	if (segment != NULL)
	{
		check(state, state->kernels->table_spill_words(JOIN_SPILL_MAX_PARTITIONS,
													   &state->shared->spill_nwords,
													   &state->status));
		state->shared->spill_words =
			dsa_allocate(area, sizeof(uint64) * state->shared->spill_nwords);
		state->shared->participants = participants;
		state->shared->lists = dsa_allocate(area, sizeof(dsa_pointer) * 2 * participants);
		state->shared->part_stats =
			dsa_allocate(area, sizeof(pg_atomic_uint64) * 2 * JOIN_SPILL_MAX_PARTITIONS);
		state->shared->rounds = InvalidDsaPointer;
		state->shared->nrounds = 0;
		state->shared->segment = dsm_segment_handle(segment);
		tess_spill_shared_init(&state->shared->fileset, segment);
	}
	else
		SharedFileSetDeleteAll(&state->shared->fileset);
	if (budget > SIZE_MAX / Max(state->shared->participants, 1))
		budget = SIZE_MAX / Max(state->shared->participants, 1);
	state->shared_budget = budget * state->shared->participants;
	check(state, state->kernels->table_spill_init(dsa_get_address(area,
																  state->shared->spill_words),
												  state->shared->spill_nwords,
												  (uint64) budget * state->shared->participants,
												  &state->status));
	for (int list = 0; list < 2 * state->shared->participants; list++)
		*participant_list(state, list / 2, list % 2 == 1) = InvalidDsaPointer;
	for (int partition = 0; partition < JOIN_SPILL_MAX_PARTITIONS; partition++)
	{
		pg_atomic_init_u64(&part_stats(state, partition)[0], 0);
		pg_atomic_init_u64(&part_stats(state, partition)[1], 0);
	}
	state->shared->spill_filter = InvalidDsaPointer;
	state->shared->spill_filter_words = 0;
	state->shared->resident_rows = 0;
	SpinLockInit(&state->shared->prune_lock);
	state->shared->prune_rows = 0;
	state->shared->prune_min = PG_INT64_MAX;
	state->shared->prune_max = PG_INT64_MIN;
	state->shared->prune_nvalues = 0;
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
		init_shared(state, pcxt->nworkers + 1, pcxt->seg);
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
		if (state->round_partition >= 0)
			round_leave(state);
		leave_shared(state, false);
		/* Its files go with the set's. */
		spill_free(state);
		free_shared_table(state);
		free_rounds(state);
		init_shared(state, state->shared->participants, NULL);
		state->built = false;
	}
	tess_shared_stats_reset(state->stats);
}

static void
join_initialize_worker(CustomScanState *css, shm_toc *toc, void *coordinate)
{
	TessHashJoinState *state = (TessHashJoinState *) css;

	if (state->shared_mode)
	{
		dsm_segment *segment;

		state->shared = coordinate;
		state->shared_budget = Min(get_hash_memory_limit(),
								   SIZE_MAX / Max(state->shared->participants, 1)) *
			state->shared->participants;
		/* The files, through the segment the worker already maps. */
		segment = dsm_find_mapping(state->shared->segment);
		if (segment == NULL)
			elog(ERROR, "TessHashJoin found no segment for its shared files");
		tess_spill_shared_attach(&state->shared->fileset, segment);
	}
	state->stats = tess_shared_stats_attach(css->ss.ps.state->es_query_cxt,
											(char *) coordinate + shared_size(state),
											ParallelWorkerNumber + 1);
}

static void
join_shutdown(CustomScanState *css)
{
	TessHashJoinState *state = (TessHashJoinState *) css;
	uint64		values[JOIN_NCOUNTERS];

	if (state->round_partition >= 0)
		round_leave(state);
	if (state->shared != NULL)
		leave_shared(state, false);
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

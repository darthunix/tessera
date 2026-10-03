/*
 * Definitions shared by the files of TessHashJoin: the executor
 * (hashjoin.c), the spill (hashjoin_spill.c) and the shared build and
 * rounds of a parallel join (hashjoin_shared.c).
 */
#ifndef TESSERA_NODES_HASHJOIN_H
#define TESSERA_NODES_HASHJOIN_H

#include "postgres.h"

#include "storage/barrier.h"
#include "utils/dsa.h"

#include "tessera/runtime.h"

#include "internal.h"

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
 * of at least JOIN_BLOOM_MIN_ROWS rows (internal.h) gets one when fewer
 * than half of them found a record.
 */
#define JOIN_BLOOM_SAMPLE 4096
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
	/* RIGHT and FULL: the marks of the table's records (join_mark_words per chunk). */
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
	/* The bytes of a record of the side (tess_table_record_size). */
	Size		record_size;
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

/* The keys of both sides and, for keys a word does not hold, their hashes. */
typedef struct JoinKeyState
{
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
} JoinKeyState;

/* The tail: the inner rows without a pair, given out after the outer side. */
typedef struct JoinTail
{
	bool		on;
	/*
	 * The tail of the table in memory is done; a spilling join asks for
	 * one before each table it drops: the resident partitions', each
	 * piece's, each partition's.
	 */
	bool		table_done;
	bool		request;
	int			chunk;
	Size		byte;
	uint32		refs[JOIN_COMPACT_ROWS];
	uint64		bits[1];
} JoinTail;

/* The Bloom filter of the inner keys, decided by a sample of the probe. */
typedef struct JoinBloom
{
	/*
	 * The Bloom filter of the table's keys, or NULL; whether the first
	 * probes decided on it, and the valid rows and matches they counted.
	 */
	uint64	   *bits;
	Size		nwords;
	bool		decided;
	/* The filter is a shared table's, and whether it was seen ready. */
	bool		shared;
	bool		ready;
	/* The outer child checks its rows against the filter: the join does not. */
	bool		below;
} JoinBloom;

/* The pruning of the outer side's partitions by the inner keys. */
typedef struct JoinPrune
{
	/*
	 * Pruning of the outer side's partitions (TessAppend, docs/nodes.md):
	 * the key's number, -1 for none, the planned descriptions and their
	 * parameters, whether the outer node took them, the keys of this build
	 * (this participant's share of a shared one) and whether the outer node
	 * has them since the last rescan.
	 */
	int			key;
	PartitionPruneInfo *values;
	PartitionPruneInfo *range;
	int			params[3];
	bool		on;
	TessJoinKeys keys;
	bool		sent;
} JoinPrune;

/* The buffers of one batch of either side. */
typedef struct JoinProbe
{
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
} JoinProbe;

/* Compact mode, for a table with duplicate keys. */
typedef struct JoinCompact
{
	/*
	 * Compact mode, for a table with duplicate keys: the pairs of the
	 * rounds are copied one after another into batches of
	 * JOIN_COMPACT_ROWS rows, the outer columns by value, the inner ones
	 * gathered from the pairs' records, instead of publishing every round
	 * over the outer batch's rows with a quarter of them selected.
	 */
	bool		on;
	/* The outer scan columns the parent asked for, all passed by value. */
	int			nouter;
	int		   *outer_columns;
	/* Per scan column: its values in the compact batch; outer ones only. */
	Datum	  **values;
	bool	  **isnull;
	uint32		offsets[JOIN_COMPACT_ROWS];
	uint64		bits[1];
	/* The round being copied: its rows not yet copied, its outer columns. */
	bool		round_open;
	uint64	   *taken_bits;
	TessDatumColumn *round_columns;
} JoinCompact;

/* The chunks of by-reference values of the table. */
typedef struct JoinValues
{
	/*
	 * The chunks of by-reference values: their bases in this process by
	 * number, with room for value_slots; the chunk values are copied into
	 * and its length and bytes used; the chunks this process made and the
	 * bytes they take.
	 */
	char	  **bases;
	int			nchunks;
	int			slots;
	int			current;
	Size		len;
	Size		used;
	int			own;
	Size		bytes;
} JoinValues;

/* A shared build, its spill and its rounds, in a parallel plan. */
typedef struct JoinParallel
{
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
} JoinParallel;

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
	JoinKeyState keys;
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
	JoinTail tail;
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
	JoinBloom bloom;
	JoinPrune prune;
	uint64		sample_rows;
	uint64		sample_found;

	JoinProbe probe;

	/* The copies of the compact batch's by-reference outer values. */
	MemoryContext compact_context;
	/* The published batch is a compact one, not a round over the outer batch. */
	bool		output_compact;
	/* The records and rows the published batch reads: a round's or a compact batch's. */
	uint32	   *current_offsets;
	uint64	   *current_bits;

	JoinCompact compact;

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

	JoinParallel parallel;
	JoinValues values;
} TessHashJoinState;

/* Raise the error a kernel stored, if the call failed. */
#define check(state, code) tess_status_check((code), &(state)->status)

/* The partition of a hash on the current level of the spill; per row. */
static inline uint32
spill_partition(const JoinSpill *spill, uint32 hash)
{
	return (hash >> spill->shift) & (uint32) (spill->npartitions - 1);
}

/* The executor (hashjoin.c): the table, its values and marks, the build and the probe. */
extern bool join_append_rows(TessHashJoinState *state, const TessTableRef *table, int chunk, TessRowMask *pending);
extern void join_batch_keys(TessHashJoinState *state, TessBatch *batch, const int *columns, const TessTableKeyKind *kinds, TessRowMask *valid);
extern void join_child_column(TessBatch *batch, int column, const TessRowMask *rows, TessColumnPurpose purpose, TessDatumColumn *result);
extern Size join_chunk_len_for(Size largest);
extern void join_decide_compact(TessHashJoinState *state);
extern void join_forget_marks(TessHashJoinState *state);
extern void join_index_table(TessHashJoinState *state);
extern Size join_memory(TessHashJoinState *state);
extern Size join_mark_words(TessHashJoinState *state);
extern void join_note_memory(TessHashJoinState *state);
extern void join_note_prune_keys(TessHashJoinState *state, TessBatch *batch);
extern dsa_pointer *join_own_list(TessHashJoinState *state, bool values);
extern dsa_pointer *join_participant_list(TessHashJoinState *state, int participant, bool values);
extern int join_prepare_inner(TessHashJoinState *state, TessBatch *batch, TessRowMask *pending);
extern void join_prune_outer(TessHashJoinState *state);
extern void join_reserve_chunks(TessHashJoinState *state, int nchunks);
extern void join_reserve_rows(TessHashJoinState *state, int nrows);
extern void join_reserve_values(TessHashJoinState *state, int nchunks);
extern void join_reset_prune_keys(TessHashJoinState *state);
extern void join_reset_values(TessHashJoinState *state);
extern void join_share_prune_keys(TessHashJoinState *state);
extern void join_shared_count(TessHashJoinState *state, int64 delta);
extern bool join_shared_on_disk(TessHashJoinState *state, uint32 partition);
extern uint32 join_shared_partitions(TessHashJoinState *state);
extern uint64 *join_shared_words(TessHashJoinState *state);
extern uint64 join_store_value(TessHashJoinState *state, Datum value, int16 typlen);
extern void join_take_back_bloom(TessHashJoinState *state);

/* The spill (hashjoin_spill.c): the partitions of both sides past hash_mem and their rounds. */
extern void join_finish_spill_build(TessHashJoinState *state);
extern void join_insert_spill(TessHashJoinState *state, TessBatch *batch);
extern uint64 join_matched_word(const JoinSpill *spill, int count);
extern bool join_next_pass(TessHashJoinState *state);
extern bool join_next_spilled(TessHashJoinState *state, JoinSpill *spill);
extern bool join_open_partition(TessHashJoinState *state, int partition);
extern void join_outer_finish(TessHashJoinState *state, TessBatch *batch);
extern TessBatch *join_outer_next(TessHashJoinState *state);
extern void join_part_open(PartReader *reader, TessSpill *file, int partition, int writers);
extern void join_side_append(TessHashJoinState *state, SpillSide *side, TessBatch *batch, TessRowMask *pending, const int *children, uint64 *nulls);
extern void join_side_compact(SpillSide *side);
extern void join_side_count(SpillSide *side, int partition, int64 delta);
extern void join_side_demote(TessHashJoinState *state, SpillSide *side, int partition);
extern void join_side_flush(TessHashJoinState *state, SpillSide *side, int partition);
extern void join_side_forget(SpillSide *side, int partition);
extern JoinSpill *join_spill_create(TessHashJoinState *state, JoinSpill *parent, double expected, uint32 shift, int forced);
extern void join_spill_free(TessHashJoinState *state);
extern Size join_spill_memory(JoinSpill *spill, uint64 *resident);
extern void join_spill_outer(TessHashJoinState *state, TessBatch *batch, TessRowMask *valid);
extern void join_split_chunk(TessHashJoinState *state, void *base, Size len, char *const *values);
extern void join_start_spill(TessHashJoinState *state);
extern bool join_tail_turn(TessHashJoinState *state);

/* The shared build and rounds of a parallel join (hashjoin_shared.c). */
extern void join_build_shared(TessHashJoinState *state);
extern void join_free_rounds(TessHashJoinState *state);
extern void join_free_shared_table(TessHashJoinState *state);
extern bool join_leave_shared(TessHashJoinState *state, bool keep);
extern pg_atomic_uint64 *join_part_stats(TessHashJoinState *state, int partition);
extern dsa_area *join_query_dsa(TessHashJoinState *state);
extern bool join_round_depart(TessHashJoinState *state);
extern void join_round_leave(TessHashJoinState *state);
extern bool join_round_next_outer(TessHashJoinState *state);
extern bool join_shared_next_partition(TessHashJoinState *state);
extern void join_shared_resident_end(TessHashJoinState *state);
extern TessBatch *join_shared_resident_next(TessHashJoinState *state);

/* Of hashjoin.c, hashjoin_begin.c and hashjoin_pairs.c, which call one another. */
extern TupleTableSlot *join_exec(CustomScanState *css);
/* The batch the node publishes reads its columns through these (hashjoin.c). */
extern const TessBatchOps join_batch_ops;
extern bool join_probe_batch(TessHashJoinState *state, TessBatch *batch);
extern bool join_next_output(TessHashJoinState *state);

#endif							/* TESSERA_NODES_HASHJOIN_H */

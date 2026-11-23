/*
 * TessAgg's key dictionaries, which number the values of a key a word
 * does not hold, and the tables of an aggregate's DISTINCT. See agg.c.
 */
#include "postgres.h"

#include "access/nbtree.h"
#include "miscadmin.h"
#include "parser/parse_agg.h"
#include "utils/fmgroids.h"
#include "utils/typcache.h"
#include "utils/lsyscache.h"
#include "utils/datum.h"
#include "utils/pg_locale.h"

#include "agg.h"
#include "agg_node.h"
#include "key_kind.h"

/* Ask memory for an address the loop reads soon, where the compiler can. */
#if defined(__GNUC__) || defined(__clang__)
#define keydict_prefetch(address) __builtin_prefetch(address)
#else
#define keydict_prefetch(address) ((void) 0)
#endif

/* A varlena whose bytes are at hand: not compressed, not external. */
static inline bool
keydict_plain(Datum value)
{
	struct varlena *pointer = (struct varlena *) DatumGetPointer(value);

	return !VARATT_IS_COMPRESSED(pointer) && !VARATT_IS_EXTERNAL(pointer);
}

/*
 * As the type's hash function hashes a value: of a key of bytes
 * (tess_key_bytes), its bytes, whatever its header, to the same hash.
 */
static pg_always_inline uint32
keydict_value_hash(KeyDict *dict, Datum value)
{
	const char *data;
	Size		len;

	/* A probe of bytes comes with its hash (agg_keydict_numbers_bytes). */
	Assert(value != PointerGetDatum(&dict->probe));
	if (dict->bytes != TESS_KEY_BYTES_NONE &&
		tess_key_value_bytes(value, dict->bytes, &data, &len))
		return hash_bytes((const unsigned char *) data, len);
	return DatumGetUInt32(FunctionCall1Coll(&dict->hashfn, dict->collation, value));
}

/*
 * Whether a probe of bytes equals a value of the dictionary: by its bytes,
 * as the type's equality under a deterministic collation compares, a value
 * the dictionary keeps compressed made plain first.
 */
static pg_noinline bool
keydict_probe_equal(KeyDict *dict, Datum value)
{
	const char *data;
	Size		len;
	struct varlena *plain;
	bool		equal;

	if (tess_key_value_bytes(value, dict->bytes, &data, &len))
		return len == dict->probe_len && memcmp(data, dict->probe_data, len) == 0;
	plain = pg_detoast_datum_packed((struct varlena *) DatumGetPointer(value));
	equal = VARSIZE_ANY_EXHDR(plain) == dict->probe_len &&
		memcmp(VARDATA_ANY(plain), dict->probe_data, dict->probe_len) == 0;
	if ((Pointer) plain != DatumGetPointer(value))
		pfree(plain);
	return equal;
}

/* Inline in the table's loops, as before char(n) made it longer. */
static pg_always_inline bool
keydict_value_equal(KeyDict *dict, Datum a, Datum b)
{
	const char *a_data;
	const char *b_data;
	Size		a_len;
	Size		b_len;

	/* simplehash compares an entry's key with the key asked, second. */
	if (unlikely(b == PointerGetDatum(&dict->probe)))
		return keydict_probe_equal(dict, a);
	if (dict->bytes != TESS_KEY_BYTES_NONE &&
		tess_key_value_bytes(a, dict->bytes, &a_data, &a_len) &&
		tess_key_value_bytes(b, dict->bytes, &b_data, &b_len))
		return a_len == b_len && memcmp(a_data, b_data, a_len) == 0;
	return DatumGetBool(FunctionCall2Coll(&dict->eqfn, dict->collation, a, b));
}

/*
 * Filled to three quarters, not simplehash's nine tenths: near that the
 * robin hood runs grow long, and a dictionary of 450 000 values made for
 * the planner's estimate of as many, 86 % full, took 17 % more of the
 * grouping (half the fill did no better).
 */
#define KEYDICT_FILLFACTOR 0.75
#define SH_FILLFACTOR (KEYDICT_FILLFACTOR)
#define SH_PREFIX keydict
#define SH_ELEMENT_TYPE KeyEntry
#define SH_KEY_TYPE Datum
#define SH_KEY value
#define SH_HASH_KEY(tb, key) keydict_value_hash((KeyDict *) (tb)->private_data, key)
#define SH_EQUAL(tb, a, b) keydict_value_equal((KeyDict *) (tb)->private_data, a, b)
#define SH_SCOPE static inline
#define SH_STORE_HASH
#define SH_GET_HASH(tb, a) a->hash
#define SH_DEFINE
#define SH_DECLARE
#include "lib/simplehash.h"

/* An index of the pairs for capacity of them in the set's memory. */
static void *
distinct_index(TessAggState *state, DistinctSet *set, uint64 capacity, Size *size)
{
	check(state, state->kernels->table_size(set->nkeys, 0, capacity,
											size, &state->status));
	return MemoryContextAllocExtended(set->context, *size, MCXT_ALLOC_HUGE);
}

/* An empty set: the groups' keys, then the argument. */
void
agg_distinct_reset(TessAggState *state, AggValue *value)
{
	DistinctSet *set = value->distinct;
	uint64		capacity = AGG_INITIAL_GROUPS;
	Size		size;

	if (set->context == NULL)
		set->context = AllocSetContextCreate(state->css.ss.ps.state->es_query_cxt,
											 "TessAgg distinct",
											 ALLOCSET_DEFAULT_SIZES);
	MemoryContextReset(set->context);
	set->nkeys = state->nkeys + 1;
	for (int key = 0; key < state->nkeys; key++)
		set->kinds[key] = state->kinds[key];
	set->kinds[state->nkeys] = value->argument_kind;
	set->capacity = 0;
	set->slots = 16;
	set->bases = MemoryContextAlloc(set->context, sizeof(void *) * set->slots);
	set->lens = MemoryContextAlloc(set->context, sizeof(Size) * set->slots);
	set->table.index = distinct_index(state, set, capacity, &size);
	set->table.index_len = size;
	set->table.chunks = set->bases;
	set->table.chunk_lens = set->lens;
	set->table.nchunks = 0;
	set->bytes = size;
	check(state, state->kernels->table_create(set->table.index, size, set->nkeys, 0, capacity,
											  &state->status));
	if (value->distinct_dict != NULL)
		agg_key_dict_reset(value->distinct_dict, 256, 1);
}

static void
distinct_add_chunk(TessAggState *state, DistinctSet *set)
{
	int			chunk = set->table.nchunks;
	Size		len = chunk == 0 ? TABLE_FIRST_CHUNK :
		table_chunk_len(state->kernels, &state->status);
	void	   *base;

	table_chunks_reserve(&set->bases, &set->lens, &set->slots, chunk + 1, set->context,
						 "TessAgg distinct table");
	set->table.chunks = set->bases;
	set->table.chunk_lens = set->lens;
	base = table_chunk_new(state->kernels, set->context, len, &state->status);
	set->bases[chunk] = base;
	set->lens[chunk] = len;
	set->table.nchunks++;
	set->bytes += len;
	note_memory(state);
}

static void
distinct_regrow(TessAggState *state, DistinctSet *set, uint64 records)
{
	void	   *old = set->table.index;
	Size		old_size = set->table.index_len;
	Size		size;
	void	   *index = distinct_index(state, set, records * 2, &size);

	check(state, state->kernels->table_regrow(&set->table, index, size,
											  records * 2, &state->status));
	set->bytes = set->bytes - old_size + size;
	set->table.index = index;
	set->table.index_len = size;
	/* The highest moment of the set: both indexes alive. */
	note_memory_alive(state, old_size);
	pfree(old);
}

/*
 * The rows of valid, of a batch of nrows rows, that insert their pair into
 * the aggregate's set: a mask in the set's buffers. group_hashes are the
 * rows' hashes of the groups' keys, state->table_keys their keys, or NULL
 * without GROUP BY.
 */
TessRowMask
agg_distinct_rows(TessAggState *state, AggValue *value, int nrows,
			  const uint32 *group_hashes, const TessRowMask *valid,
			  const TessDatumColumn *argument)
{
	DistinctSet *set = value->distinct;
	int			nwords = tess_row_mask_word_count(nrows);
	TessTableKey keys[TESS_TABLE_MAX_KEYS];
	TessRowMask pending;
	TessRowMask inserted;
	TessDatumColumn numbers;

	/* A value a word does not hold: the pairs take its number in the dictionary. */
	if (value->distinct_dict != NULL)
	{
		KeyDict    *dict = value->distinct_dict;

		if (dict->capacity < nrows)
		{
			MemoryContext query = state->css.ss.ps.state->es_query_cxt;

			dict->capacity = nrows;
			dict->batch_numbers = MemoryContextAlloc(query, sizeof(Datum) * nrows);
			dict->batch_hashes = MemoryContextAlloc(query, sizeof(uint32) * nrows);
		}
		agg_keydict_numbers(dict, argument, valid, true, dict->batch_numbers, dict->batch_hashes);
		numbers = *argument;
		numbers.values = dict->batch_numbers;
		argument = &numbers;
	}
	if (set->capacity < nrows)
	{
		set->hashes = MemoryContextAlloc(set->context, sizeof(uint32) * nrows);
		set->offsets = MemoryContextAlloc(set->context, sizeof(uint32) * nrows);
		set->pending_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
		set->inserted_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
		set->capacity = nrows;
	}
	pending = (TessRowMask) {nrows, set->pending_bits};
	inserted = (TessRowMask) {nrows, set->inserted_bits};
	memset(set->inserted_bits, 0, sizeof(uint64) * nwords);
	/* The groups' hashes folded with the argument's; a NULL one drops out. */
	if (group_hashes != NULL)
	{
		memcpy(set->hashes, group_hashes, sizeof(uint32) * nrows);
		memcpy(set->pending_bits, valid->bits, sizeof(uint64) * nwords);
		check(state, tess_key_hash_next(state->kernels, value->argument_kind) (argument, NULL,
													   TESS_NULL_KEYS_REJECT,
													   set->hashes, &pending,
													   &state->status));
	}
	else
	{
		memset(set->pending_bits, 0, sizeof(uint64) * nwords);
		check(state, tess_key_hash(state->kernels, value->argument_kind) (argument, NULL, valid,
												  TESS_NULL_KEYS_REJECT,
												  set->hashes, &pending,
												  &state->status));
	}
	for (int key = 0; key < state->nkeys; key++)
		keys[key] = state->table_keys[key];
	keys[state->nkeys].kind = value->argument_kind;
	keys[state->nkeys].column = argument;
	keys[state->nkeys].prepared = NULL;
	if (set->table.nchunks == 0)
		distinct_add_chunk(state, set);
	for (;;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

		/* Each call adds the pairs it makes to inserted, cleared above. */
		check(state, state->kernels->table_find_or_insert(&set->table,
														  set->table.nchunks - 1,
														  set->hashes, set->nkeys,
														  keys, &pending,
														  set->offsets, &inserted,
														  &state->status));
		if (tess_row_mask_count(&pending) == 0)
			break;
		check(state, state->kernels->table_stats(&set->table, &stats,
												 &state->status));
		if (stats.records * 2 >= stats.buckets)
			distinct_regrow(state, set, stats.records);
		else
			distinct_add_chunk(state, set);
	}
	return inserted;
}

/* The bytes of every distinct set. */
Size
agg_distinct_bytes(TessAggState *state)
{
	Size		bytes = 0;

	for (int index = 0; index < state->nvalues; index++)
	{
		AggValue   *value = &state->values[index];

		if (value->distinct != NULL)
			bytes += value->distinct->bytes;
		if (value->distinct_dict != NULL)
			bytes += MemoryContextMemAllocated(value->distinct_dict->context, true);
	}
	return bytes;
}

/* ------------------------------------------------------ keys through dictionaries */

/*
 * Whether equal values of the key's type are equal bytes: compared by
 * them (text under a deterministic collation, bytea), or so under any
 * collation by the type's default B-tree family (btequalimage: oid, uuid,
 * ...). Not for numeric, float, text under a nondeterministic collation
 * (text_pattern_ops says so of its bytewise order only), bpchar, whose
 * equality ignores trailing blanks, arrays, ranges or a family that does
 * not say.
 */
static bool
key_dict_images_equal(KeyDict *dict, Oid type)
{
	TypeCacheEntry *entry;

	if (dict->bytes == TESS_KEY_BYTES_WHOLE)
		return true;
	entry = lookup_type_cache(type, TYPECACHE_BTREE_OPFAMILY);
	return OidIsValid(entry->btree_opf) &&
		get_opfamily_proc(entry->btree_opf, entry->btree_opintype, entry->btree_opintype,
						  BTEQUALIMAGE_PROC) == F_BTEQUALIMAGE;
}

KeyDict *
agg_key_dict_create(TessAggState *state, Oid eqop, Oid type, Oid collation)
{
	EState	   *estate = state->css.ss.ps.state;
	KeyDict    *dict = MemoryContextAllocZero(estate->es_query_cxt, sizeof(KeyDict));
	RegProcedure hashproc;

	if (!get_op_hash_functions(eqop, &hashproc, NULL))
		elog(ERROR, "TessAgg found no hash function of operator %u", eqop);
	fmgr_info_cxt(get_opcode(eqop), &dict->eqfn, estate->es_query_cxt);
	fmgr_info_cxt(hashproc, &dict->hashfn, estate->es_query_cxt);
	dict->collation = collation;
	dict->bytes = tess_key_bytes(hashproc, get_opcode(eqop), collation);
	get_typlenbyval(type, &dict->typlen, &dict->typbyval);
	dict->forms = state->nkeys > 1 && !key_dict_images_equal(dict, type);
	dict->context = AllocSetContextCreate(estate->es_query_cxt, "TessAgg key values",
										  ALLOCSET_DEFAULT_SIZES);
	return dict;
}

/*
 * Forget every value: the table they numbered is made anew, for one of
 * `shares` dictionaries of the node's keys.
 */
void
agg_key_dict_reset(KeyDict *dict, uint64 values, int shares)
{
	MemoryContextReset(dict->context);
	/*
	 * Room for the values expected, where the planner's estimate of the
	 * groups is known and within a quarter of hash_mem for every
	 * dictionary of the keys together, which leaves the table its own
	 * room: a dictionary grown from 256 to half a million values took 5 %
	 * of an INTERSECT.
	 */
	values = Min(values, tess_hash_memory_limit() / 4 / Max(shares, 1) /
				 (sizeof(KeyEntry) * 2 / KEYDICT_FILLFACTOR + sizeof(Datum)));
	values = Max(values, 256);
	dict->table = keydict_create(dict->context, values, dict);
	dict->slots = values;
	dict->values = MemoryContextAlloc(dict->context, sizeof(Datum) * dict->slots);
	dict->count = 0;
	dict->block = NULL;
	dict->block_used = 0;
	dict->block_len = 0;
	dict->form_table = NULL;
	/* The numbers of a source's indexes: none until a batch names one. */
	dict->index_identity = 0;
	dict->index_count = 0;
	dict->index_entries = NULL;
	dict->index_share = tess_hash_memory_limit() / 8 / Max(shares, 1);
}

#define KEYDICT_BLOCK_LEN (64 * 1024)
#define KEYDICT_PREFETCH_BYTES (1024 * 1024)

/*
 * A copy of a by-reference value (tess_value_copy) in the dictionary's
 * blocks, one after another, each at a MAXALIGN'd place; a value past a
 * quarter of a block is a copy of its own.
 */
static Datum
keydict_copy(KeyDict *dict, Datum value)
{
	ExpandedObjectHeader *expanded;
	Size		size;
	char	   *copy;

	if (dict->typbyval)
		return value;
	size = tess_value_size(value, dict->typlen, &expanded);
	if (size > KEYDICT_BLOCK_LEN / 4)
		copy = palloc(size);
	else
	{
		if (dict->block == NULL || dict->block_used + size > dict->block_len)
		{
			dict->block = MemoryContextAlloc(dict->context, KEYDICT_BLOCK_LEN);
			dict->block_len = KEYDICT_BLOCK_LEN;
			dict->block_used = 0;
		}
		copy = dict->block + dict->block_used;
		dict->block_used += MAXALIGN(size);
	}
	tess_value_copy(value, expanded, copy, size);
	return PointerGetDatum(copy);
}

/*
 * A copy of a value of a key of bytes from its bytes: a header and the
 * bytes, as a Datum of the type holds them, in the dictionary's blocks as
 * keydict_copy places a Datum's.
 */
static Datum
keydict_copy_bytes(KeyDict *dict, const char *data, uint32 len)
{
	Size		size = VARHDRSZ + (Size) len;
	char	   *copy;

	if (size > KEYDICT_BLOCK_LEN / 4)
		copy = palloc(size);
	else
	{
		if (dict->block == NULL || dict->block_used + size > dict->block_len)
		{
			dict->block = MemoryContextAlloc(dict->context, KEYDICT_BLOCK_LEN);
			dict->block_len = KEYDICT_BLOCK_LEN;
			dict->block_used = 0;
		}
		copy = dict->block + dict->block_used;
		dict->block_used += MAXALIGN(size);
	}
	SET_VARSIZE(copy, size);
	memcpy(VARDATA(copy), data, len);
	return PointerGetDatum(copy);
}

/*
 * A new value's number: the next, its copy kept by it in the dictionary's
 * context, made from data and len where data is given, else from value.
 */
static pg_always_inline void
keydict_enter(KeyDict *dict, KeyEntry *entry, Datum value, const char *data, uint32 len)
{
	MemoryContext old = MemoryContextSwitchTo(dict->context);

	if (dict->count == dict->slots)
	{
		if (dict->count == PG_UINT32_MAX)
			elog(ERROR, "TessAgg numbers at most %u values of a key", PG_UINT32_MAX);
		dict->slots *= 2;
		dict->values = repalloc_huge(dict->values, sizeof(Datum) * dict->slots);
	}
	entry->value = data != NULL ? keydict_copy_bytes(dict, data, len) :
		keydict_copy(dict, value);
	entry->number = (uint32) dict->count;
	dict->values[dict->count++] = entry->value;
	MemoryContextSwitchTo(old);
}

/*
 * A dictionary of values past the caches: each row's bucket asked of
 * memory before the lookups, so that the misses of a batch's rows overlap
 * rather than each lookup waiting for its own (half a million values of
 * text: 8 to 11 % of the query). A pass of its own, the hashes computed
 * again by the lookups, and only by the values held, not the buckets: a
 * table made for the planner's estimate of 427 000 groups held 99 values,
 * which the pass cost 8 %.
 */
static pg_noinline void
keydict_prefetch_rows(KeyDict *dict, const TessDatumColumn *column, const TessRowMask *rows)
{
	int			row = -1;

	while ((row = tess_row_mask_next(rows, row)) >= 0)
		if (!column->isnull[row])
			keydict_prefetch(&dict->table->data[keydict_value_hash(dict, column->values[row]) &
												 dict->table->sizemask]);
}

/*
 * The numbers of the indexes of a source's dictionary of identity and
 * nvalues values, all unseen: no more entries than its values, nor than the
 * key's share; the array of an identity before is freed.
 */
static void
keydict_index_reset(KeyDict *dict, uint64 identity, uint32 nvalues)
{
	Size		count = Min((Size) nvalues, dict->index_share / sizeof(KeyIndexEntry));

	if (dict->index_entries != NULL)
		pfree(dict->index_entries);
	dict->index_entries = NULL;
	dict->index_count = 0;
	dict->index_identity = identity;
	if (count == 0)
		return;
	dict->index_entries = MemoryContextAllocExtended(dict->context,
													 mul_size(count, sizeof(KeyIndexEntry)),
													 MCXT_ALLOC_HUGE);
	for (Size index = 0; index < count; index++)
		dict->index_entries[index].number = KEY_INDEX_UNSEEN;
	dict->index_count = (uint32) count;
}

/* The entry of a row's index of a source's dictionary, or NULL past the array. */
static inline KeyIndexEntry *
keydict_index_of(KeyDict *dict, const TessBytesColumn *bytes, int row)
{
	int32		index;

	if (bytes == NULL || bytes->indexes == NULL || bytes->dictionary == 0 ||
		bytes->dictionary != dict->index_identity)
		return NULL;
	index = bytes->indexes[row];
	return index >= 0 && (uint32) index < dict->index_count ?
		&dict->index_entries[index] : NULL;
}

#ifdef USE_ASSERT_CHECKING
/* A number found by an index holds the row's bytes. */
static void
keydict_check_index(KeyDict *dict, uint32 number, const char *data, uint32 len)
{
	dict->probe_data = data;
	dict->probe_len = len;
	if (number >= dict->count || !keydict_probe_equal(dict, dict->values[number]))
		elog(ERROR, "TessAgg found by a source's index a number of other bytes");
}
#endif

/*
 * The numbers of the rows of rows, of a column of Datums or, where bytes
 * is given, of a column in the bytes form, the hashes of the latter's
 * rows already in hashes where hashed: one loop for both forms, so that
 * the table's lookup and insertion have one caller each, which the
 * compiler inlines into it (a second caller made it call them for every
 * row, 2 % of a COUNT(DISTINCT) of strings).
 */
static void
keydict_numbers(KeyDict *dict, const TessDatumColumn *datums, const TessBytesColumn *bytes,
				bool hashed, const TessRowMask *rows, bool insert, Datum *numbers,
				uint32 *hashes)
{
	Datum		probe = PointerGetDatum(&dict->probe);
	int			row = -1;

	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		KeyIndexEntry *cached = NULL;
		Datum		key;
		uint32		hash;
		int64		number;

		if (bytes != NULL ? bytes->isnull[row] : datums->isnull[row])
		{
			numbers[row] = (Datum) 0;
			hashes[row] = 0;
			continue;
		}
		if (bytes != NULL)
		{
			/* A source's index met before: its number and hash. */
			cached = keydict_index_of(dict, bytes, row);
			if (cached != NULL && cached->number != KEY_INDEX_UNSEEN)
			{
#ifdef USE_ASSERT_CHECKING
				keydict_check_index(dict, cached->number, bytes->data[row], bytes->lengths[row]);
#endif
				numbers[row] = Int64GetDatum((int64) cached->number);
				hashes[row] = cached->hash;
				continue;
			}
			dict->probe_data = bytes->data[row];
			dict->probe_len = bytes->lengths[row];
			key = probe;
			hash = hashed ? hashes[row] :
				hash_bytes((const unsigned char *) dict->probe_data, (int) dict->probe_len);
		}
		else
		{
			key = datums->values[row];
			hash = keydict_value_hash(dict, key);
		}
		hashes[row] = hash;
		if (insert)
		{
			bool		found;
			KeyEntry   *entry = keydict_insert_hash(dict->table, key, hash, &found);

			if (!found)
			{
				if (bytes != NULL)
					keydict_enter(dict, entry, (Datum) 0, dict->probe_data, dict->probe_len);
				else
					keydict_enter(dict, entry, key, NULL, 0);
			}
			number = entry->number;
		}
		else
		{
			KeyEntry   *entry = keydict_lookup_hash(dict->table, key, hash);

			number = entry != NULL ? (int64) entry->number : -1;
		}
		numbers[row] = Int64GetDatum(number);
		/* A value the dictionary holds: its index's number from now on. */
		if (cached != NULL && number >= 0)
		{
			cached->number = (uint32) number;
			cached->hash = hash;
		}
	}
}

/*
 * The numbers of the values of the rows of rows: a value the dictionary
 * lacks gets the next number, or, with insert false, -1, which no record
 * has; NULL stays NULL for the table's NULL key. hashes[row] receives the
 * value's hash, 0 for NULL.
 */
void
agg_keydict_numbers(KeyDict *dict, const TessDatumColumn *column, const TessRowMask *rows,
				bool insert, Datum *numbers, uint32 *hashes)
{
	if (dict->table->members * sizeof(KeyEntry) > KEYDICT_PREFETCH_BYTES)
		keydict_prefetch_rows(dict, column, rows);
	keydict_numbers(dict, column, NULL, false, rows, insert, numbers, hashes);
}

/*
 * As agg_keydict_numbers, of the rows of a column in the bytes form, of a
 * key of bytes (TESS_KEY_BYTES_WHOLE): each value looked up by its bytes,
 * with the hash and the equality its Datum would get, so that rows of
 * either form find one number; a value the dictionary lacks enters as a
 * copy made from its bytes.
 */
void
agg_keydict_numbers_bytes(KeyDict *dict, const TessBytesColumn *column,
						  const TessRowMask *rows, bool insert, Datum *numbers,
						  uint32 *hashes)
{
	bool		hashed = dict->table->members * sizeof(KeyEntry) > KEYDICT_PREFETCH_BYTES;
	int			row = -1;

	Assert(dict->bytes == TESS_KEY_BYTES_WHOLE);
	/* A source's dictionary of another identity: its indexes' numbers anew. */
	if (column->dictionary != 0 && column->indexes != NULL &&
		dict->index_identity != column->dictionary)
		keydict_index_reset(dict, column->dictionary, column->nvalues);
	/*
	 * Past the caches, each bucket asked of memory first, by the hash kept;
	 * not of a row whose index is met before, which looks nothing up.
	 */
	if (hashed)
		while ((row = tess_row_mask_next(rows, row)) >= 0)
		{
			KeyIndexEntry *cached = keydict_index_of(dict, column, row);

			if (!column->isnull[row] &&
				(cached == NULL || cached->number == KEY_INDEX_UNSEEN))
			{
				hashes[row] = hash_bytes((const unsigned char *) column->data[row],
										 (int) column->lengths[row]);
				keydict_prefetch(&dict->table->data[hashes[row] & dict->table->sizemask]);
			}
		}
	keydict_numbers(dict, NULL, column, hashed, rows, insert, numbers, hashes);
}

/* As datum_image_eq, without its calls for a value whose bytes are at hand. */
static inline bool
keydict_same_image(KeyDict *dict, Datum a, Datum b)
{
	if (dict->typbyval)
		return a == b;
	if (dict->typlen == -1 && keydict_plain(a) && keydict_plain(b))
	{
		Size		len = VARSIZE_ANY_EXHDR(DatumGetPointer(a));

		return len == VARSIZE_ANY_EXHDR(DatumGetPointer(b)) &&
			memcmp(VARDATA_ANY(DatumGetPointer(a)), VARDATA_ANY(DatumGetPointer(b)),
				   len) == 0;
	}
	return datum_image_eq(a, b, dict->typbyval, dict->typlen);
}

/*
 * The groups the rows of inserted made, by keys of several forms: a group
 * whose row came in another form than its number's first keeps a copy of
 * it by its record. Once per group, so a copy per group at most.
 */
void
agg_key_forms(TessAggState *state, const TessRowMask *inserted)
{
	for (int key = 0; key < state->nkeys; key++)
	{
		KeyDict    *dict = state->dicts[key];
		const TessDatumColumn *column = &state->key_columns[key];
		int			row = -1;

		if (dict == NULL || !dict->forms)
			continue;
		while ((row = tess_row_mask_next(inserted, row)) >= 0)
		{
			Datum		value = column->values[row];
			MemoryContext old;
			KeyForm    *form;
			bool		found;

			if (column->isnull[row] ||
				keydict_same_image(dict, value,
								   dict->values[DatumGetInt64(dict->batch_numbers[row])]))
				continue;
			old = MemoryContextSwitchTo(dict->context);
			if (dict->form_table == NULL)
				dict->form_table = keyform_create(dict->context, 64, NULL);
			form = keyform_insert(dict->form_table, state->offsets[row], &found);
			form->value = keydict_copy(dict, value);
			MemoryContextSwitchTo(old);
		}
	}
}

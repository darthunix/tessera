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

	if (dict->bytes != TESS_KEY_BYTES_NONE &&
		tess_key_value_bytes(value, dict->bytes, &data, &len))
		return hash_bytes((const unsigned char *) data, len);
	return DatumGetUInt32(FunctionCall1Coll(&dict->hashfn, dict->collation, value));
}

/* Inline in the table's loops, as before char(n) made it longer. */
static pg_always_inline bool
keydict_value_equal(KeyDict *dict, Datum a, Datum b)
{
	const char *a_data;
	const char *b_data;
	Size		a_len;
	Size		b_len;

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

/* An empty set: with GROUP BY the group's record, then the argument. */
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
	set->nkeys = state->nkeys > 0 ? 2 : 1;
	set->kinds[0] = TESS_TABLE_KEY_INT8;
	set->kinds[set->nkeys - 1] = value->argument_kind;
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

/* The set's buffers of a batch of nrows rows. */
static void
distinct_reserve(DistinctSet *set, int nrows)
{
	int			nwords = tess_row_mask_word_count(nrows);

	if (set->capacity >= nrows)
		return;
	set->refs = MemoryContextAlloc(set->context, sizeof(Datum) * nrows);
	set->ref_nulls = MemoryContextAllocZero(set->context, sizeof(bool) * nrows);
	set->hashes = MemoryContextAlloc(set->context, sizeof(uint32) * nrows);
	set->offsets = MemoryContextAlloc(set->context, sizeof(uint32) * nrows);
	set->pending_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
	set->inserted_bits = MemoryContextAlloc(set->context, sizeof(uint64) * nwords);
	set->capacity = nrows;
}

/*
 * The rows of pending, hashed into set->hashes and keyed by keys, into the
 * set: inserted, cleared, receives those that made a pair. A frozen set
 * takes none: a row whose pair it has drops, and the others go to disk,
 * with the references of set->refs and the values of keys' argument.
 */
static void
distinct_insert(TessAggState *state, DistinctSet *set, const TessTableKey *keys,
				TessRowMask *pending, TessRowMask *inserted)
{
	if (set->pairs != NULL && set->pairs->writing != NULL)
	{
		int			nwords = tess_row_mask_word_count(pending->nrows);

		check(state, state->kernels->table_probe(&set->table, set->hashes, set->nkeys, keys,
												 pending, set->offsets, inserted,
												 &state->status));
		for (int word = 0; word < nwords; word++)
		{
			pending->bits[word] &= ~inserted->bits[word];
			inserted->bits[word] = 0;
		}
		pairs_write(state, set->pairs, pending, set->refs, keys[1].column->values, set->hashes);
		return;
	}
	if (set->table.nchunks == 0)
		distinct_add_chunk(state, set);
	for (;;)
	{
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);

		/* Each call adds the pairs it makes to inserted, cleared by the caller. */
		check(state, state->kernels->table_find_or_insert(&set->table,
														  set->table.nchunks - 1,
														  set->hashes, set->nkeys,
														  keys, pending,
														  set->offsets, inserted,
														  &state->status));
		if (tess_row_mask_count(pending) == 0)
			break;
		check(state, state->kernels->table_stats(&set->table, &stats,
												 &state->status));
		if (stats.records * 2 >= stats.buckets)
			distinct_regrow(state, set, stats.records);
		else
			distinct_add_chunk(state, set);
	}
}

/*
 * The rows of valid, of a batch of nrows rows, that insert their pair into
 * the aggregate's set: a mask in the set's buffers. group_hashes are the
 * rows' hashes of the groups' keys and group_refs the references of their
 * groups' records, or both NULL without GROUP BY.
 */
TessRowMask
agg_distinct_rows(TessAggState *state, AggValue *value, int nrows,
			  const uint32 *group_hashes, const uint32 *group_refs,
			  const TessRowMask *valid, const TessDatumColumn *argument)
{
	DistinctSet *set = value->distinct;
	int			nwords = tess_row_mask_word_count(nrows);
	TessTableKey keys[2];
	TessRowMask pending;
	TessRowMask inserted;
	TessDatumColumn numbers;
	TessDatumColumn refs = TESS_STRUCT_INITIALIZER(TessDatumColumn);

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
	distinct_reserve(set, nrows);
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
	/*
	 * The groups' references, of every row: a key column without a mask of
	 * the rows ready is read on the table's quick path.
	 */
	Assert((group_refs != NULL) == (set->nkeys == 2));
	if (group_refs != NULL)
	{
		/* A local: a store through set->refs could change it, as it may alias. */
		Datum	   *ref_words = set->refs;

		for (int row = 0; row < nrows; row++)
			ref_words[row] = (Datum) group_refs[row];
		refs.values = set->refs;
		refs.isnull = set->ref_nulls;
		refs.nrows = nrows;
		keys[0] = (TessTableKey) {TESS_TABLE_KEY_INT8, &refs, NULL};
	}
	keys[set->nkeys - 1].kind = value->argument_kind;
	keys[set->nkeys - 1].column = argument;
	keys[set->nkeys - 1].prepared = NULL;
	distinct_insert(state, set, keys, &pending, &inserted);
	return inserted;
}

/*
 * The nrows pairs of a partition read back (pairs_read) into the set of
 * value, under the hashes they were written with: the rows that make a
 * pair, a mask in the set's buffers. A set frozen again sends the others
 * to the level below.
 */
TessRowMask
agg_distinct_read(TessAggState *state, AggValue *value, int nrows, Datum *const *columns)
{
	DistinctSet *set = value->distinct;
	int			nwords = tess_row_mask_word_count(nrows);
	TessDatumColumn refs = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessDatumColumn values = TESS_STRUCT_INITIALIZER(TessDatumColumn);
	TessTableKey keys[2];
	TessRowMask pending;
	TessRowMask inserted;

	Assert(set->nkeys == 2 && nrows <= AGG_GROUP_ROWS);
	distinct_reserve(set, nrows);
	pending = (TessRowMask) {nrows, set->pending_bits};
	inserted = (TessRowMask) {nrows, set->inserted_bits};
	memset(set->inserted_bits, 0, sizeof(uint64) * nwords);
	set->pending_bits[0] = nrows == 64 ? UINT64_MAX : (UINT64CONST(1) << nrows) - 1;
	for (int row = 0; row < nrows; row++)
	{
		set->refs[row] = columns[0][row];
		set->hashes[row] = (uint32) columns[2][row];
	}
	refs.values = set->refs;
	refs.isnull = set->ref_nulls;
	refs.nrows = nrows;
	values.values = columns[1];
	values.isnull = set->ref_nulls;
	values.nrows = nrows;
	keys[0] = (TessTableKey) {TESS_TABLE_KEY_INT8, &refs, NULL};
	keys[1] = (TessTableKey) {value->argument_kind, &values, NULL};
	distinct_insert(state, set, keys, &pending, &inserted);
	return inserted;
}

/*
 * After a batch, the groups taking groups bytes: the machine of each set
 * that spills, or of only's alone, weighs the node's memory and the set's
 * pairs, and may freeze the set; a machine that weighed the same memory
 * with a batch of pairs in the set is not asked again.
 */
void
agg_distinct_grown(TessAggState *state, AggValue *only, Size groups)
{
	for (int index = 0; index < state->nvalues; index++)
	{
		AggValue   *value = &state->values[index];
		DistinctSet *set = value->distinct;
		TessTableStats stats = TESS_STRUCT_INITIALIZER(TessTableStats);
		Size		memory;

		if (set == NULL || set->pairs == NULL || set->pairs->writing != NULL ||
			(only != NULL && value != only) || set->table.nchunks == 0)
			continue;
		memory = groups + agg_distinct_bytes(state);
		if (memory == set->pairs->weighed)
			continue;
		check(state, state->kernels->table_stats(&set->table, &stats, &state->status));
		pairs_grown(state, set->pairs, memory, stats.records);
	}
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
		if (value->distinct != NULL && value->distinct->pairs != NULL)
			bytes += pairs_memory(value->distinct->pairs);
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
 * The numbers of the values of the rows of rows: a value the dictionary
 * lacks gets the next number, or, with insert false, -1, which no record
 * has; NULL stays NULL for the table's NULL key. hashes[row] receives the
 * value's hash, 0 for NULL.
 */
void
agg_keydict_numbers(KeyDict *dict, const TessDatumColumn *column, const TessRowMask *rows,
				bool insert, Datum *numbers, uint32 *hashes)
{
	int			row = -1;

	if (dict->table->members * sizeof(KeyEntry) > KEYDICT_PREFETCH_BYTES)
		keydict_prefetch_rows(dict, column, rows);
	while ((row = tess_row_mask_next(rows, row)) >= 0)
	{
		Datum		value = column->values[row];
		uint32		hash;

		if (column->isnull[row])
		{
			numbers[row] = (Datum) 0;
			hashes[row] = 0;
			continue;
		}
		hash = keydict_value_hash(dict, value);
		hashes[row] = hash;
		if (insert)
		{
			bool		found;
			KeyEntry   *entry = keydict_insert_hash(dict->table, value, hash, &found);

			if (!found)
			{
				MemoryContext old = MemoryContextSwitchTo(dict->context);

				if (dict->count == dict->slots)
				{
					if (dict->count == PG_UINT32_MAX)
						elog(ERROR, "TessAgg numbers at most %u values of a key", PG_UINT32_MAX);
					dict->slots *= 2;
					dict->values = repalloc_huge(dict->values, sizeof(Datum) * dict->slots);
				}
				entry->value = keydict_copy(dict, value);
				entry->number = (uint32) dict->count;
				dict->values[dict->count++] = entry->value;
				MemoryContextSwitchTo(old);
			}
			numbers[row] = Int64GetDatum(entry->number);
		}
		else
		{
			KeyEntry   *entry = keydict_lookup_hash(dict->table, value, hash);

			numbers[row] = Int64GetDatum(entry != NULL ? (int64) entry->number : -1);
		}
	}
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

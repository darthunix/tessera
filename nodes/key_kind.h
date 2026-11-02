/*
 * The kinds of the keys the nodes put in the hash table, in one place:
 * which type takes which kind, the kind of a value put in as a number,
 * which kinds the table keeps, and how a key of each kind is hashed and
 * bounded. A new kind of key adds its cases here, beside the C API's
 * kinds (tessera/table_key.h); the nodes call these and compare no kind
 * themselves.
 */
#ifndef TESSERA_NODES_KEY_KIND_H
#define TESSERA_NODES_KEY_KIND_H

#include "catalog/pg_type_d.h"
#include "utils/fmgroids.h"
#include "utils/pg_locale.h"
#include "varatt.h"

#include "tessera/kernel_ops.h"
#include "tessera/table_key.h"

#include "abbrev.h"

/*
 * The kind of a value put in the table as a number of 64 bits: its number
 * in a grouping's dictionary, its hash from the type's function in a
 * join, or a sort key's abbreviated word.
 */
#define TESS_KEY_KIND_NUMBER TESS_TABLE_KEY_INT8

/*
 * The table's kind of a key of this type: a value its word holds whole and
 * compares bit for bit, sign-extended to eight bytes as the kernels hash,
 * order and give it back, as PostgreSQL's Datum of the type is: int2,
 * int4, date and bool as INT4, int8, timestamp and timestamptz as INT8.
 * False for any other type: its values go by reference, or compare other
 * than bit for bit, or come back unlike the core's Datum (oid).
 */
static inline bool
tess_word_key_kind(Oid type, TessTableKeyKind *kind)
{
	switch (type)
	{
		case INT2OID:
		case INT4OID:
		case DATEOID:
		case BOOLOID:
			*kind = TESS_TABLE_KEY_INT4;
			return true;
		case INT8OID:
		case TIMESTAMPOID:
		case TIMESTAMPTZOID:
			*kind = TESS_TABLE_KEY_INT8;
			return true;
		default:
			return false;
	}
}

/* Whether the table keeps keys of this kind: a plan's kinds are checked. */
static inline bool
tess_key_kind_known(int kind)
{
	return kind == TESS_TABLE_KEY_INT4 || kind == TESS_TABLE_KEY_INT8;
}

/* The kind of a sort key's word: a generic key's is its abbreviated word. */
static inline TessTableKeyKind
tess_key_kind_of_sort(int kind)
{
	return kind == TESS_SORT_KIND_GENERIC ? TESS_KEY_KIND_NUMBER : (TessTableKeyKind) kind;
}

/* The kernel that hashes the first key of a batch, of this kind. */
static inline __typeof__(tess_int4_hash) *
tess_key_hash(const TessKernelOps *kernels, TessTableKeyKind kind)
{
	return kind == TESS_TABLE_KEY_INT8 ? kernels->int8_hash : kernels->int4_hash;
}

/* The kernel that folds a further key of this kind into the hashes. */
static inline __typeof__(tess_int4_hash_next) *
tess_key_hash_next(const TessKernelOps *kernels, TessTableKeyKind kind)
{
	return kind == TESS_TABLE_KEY_INT8 ? kernels->int8_hash_next : kernels->int4_hash_next;
}

/* Whether a key of this kind is 8 bytes, as a pruning join reads its bounds. */
static inline bool
tess_key_kind_wide(TessTableKeyKind kind)
{
	return kind == TESS_TABLE_KEY_INT8;
}

/*
 * Of a key a word does not hold, the bytes its type's hash and equality
 * read, where they read bytes: the whole value (text and varchar under a
 * deterministic collation, bytea), or the value without its trailing
 * blanks (char(n) under a deterministic collation). A node then hashes
 * and compares those bytes itself, as the type's functions would, to the
 * same hash, and calls the functions only for a value compressed or
 * stored outside the row (tess_key_value_bytes).
 */
typedef enum TessKeyBytes
{
	TESS_KEY_BYTES_NONE,		/* the type's functions, always */
	TESS_KEY_BYTES_WHOLE,		/* every byte of the value */
	TESS_KEY_BYTES_TRIMMED		/* the bytes before its trailing blanks */
} TessKeyBytes;

/*
 * The bytes of a key hashed by hashproc (the 32-bit or the extended hash
 * of the type) under collation; with equality valid, the key's equality
 * function must also be the one of those bytes, as a grouping compares
 * by it, while a join checks its equality apart.
 */
static inline TessKeyBytes
tess_key_bytes(Oid hashproc, Oid equality, Oid collation)
{
	TessKeyBytes form;
	Oid			bytes_equality;

	switch (hashproc)
	{
		/* bytea's own since PostgreSQL 18, which call hashvarlena's. */
#ifdef F_HASHBYTEA
		case F_HASHBYTEA:
		case F_HASHBYTEAEXTENDED:
#endif
		case F_HASHVARLENA:
		case F_HASHVARLENAEXTENDED:
			form = TESS_KEY_BYTES_WHOLE;
			bytes_equality = F_BYTEAEQ;
			break;
		case F_HASHTEXT:
		case F_HASHTEXTEXTENDED:
			form = TESS_KEY_BYTES_WHOLE;
			bytes_equality = F_TEXTEQ;
			break;
		case F_HASHBPCHAR:
		case F_HASHBPCHAREXTENDED:
			form = TESS_KEY_BYTES_TRIMMED;
			bytes_equality = F_BPCHAREQ;
			break;
		default:
			return TESS_KEY_BYTES_NONE;
	}
	if (OidIsValid(equality) && equality != bytes_equality)
		return TESS_KEY_BYTES_NONE;
	/* Text and char(n) hash by bytes only where equal means equal bytes. */
	if (bytes_equality != F_BYTEAEQ &&
		(!OidIsValid(collation) || !pg_newlocale_from_collation(collation)->deterministic))
		return TESS_KEY_BYTES_NONE;
	return form;
}

/*
 * The bytes of a value of a key of form, not NONE, as its type's
 * functions read them; false for a value compressed or stored outside
 * the row, which goes to the functions.
 */
static inline bool
tess_key_value_bytes(Datum value, TessKeyBytes form, const char **data, Size *len)
{
	struct varlena *pointer = (struct varlena *) DatumGetPointer(value);

	if (VARATT_IS_COMPRESSED(pointer) || VARATT_IS_EXTERNAL(pointer))
		return false;
	*data = VARDATA_ANY(pointer);
	*len = VARSIZE_ANY_EXHDR(pointer);
	/* As bcTruelen: only the ASCII blank, a unit of every server encoding. */
	if (form == TESS_KEY_BYTES_TRIMMED)
		while (*len > 0 && (*data)[*len - 1] == ' ')
			(*len)--;
	return true;
}

#endif

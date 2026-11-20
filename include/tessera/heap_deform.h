/* Incremental deformation of one heap tuple, attribute by attribute. */
#ifndef TESSERA_HEAP_DEFORM_H
#define TESSERA_HEAP_DEFORM_H

#include "postgres.h"

#include "access/htup_details.h"
#include "access/tupdesc.h"
#include "access/tupmacs.h"

#if PG_VERSION_NUM < 190000
/*
 * PostgreSQL 18 lacks the helpers of 19's deformation: these are 19's own,
 * from c.h and access/tupmacs.h.
 */
#include "port/pg_bitutils.h"

#ifndef pg_assume
#if defined(USE_ASSERT_CHECKING)
#define pg_assume(expr) Assert(expr)
#elif defined(HAVE__BUILTIN_UNREACHABLE)
#define pg_assume(expr) \
	do { \
		if (!(expr)) \
			__builtin_unreachable(); \
	} while (0)
#else
#define pg_assume(expr) ((void) 0)
#endif
#endif

/* fetch_att without its check of a by-value length: CompactAttribute's is valid. */
static inline Datum
fetch_att_noerr(const void *T, bool attbyval, int attlen)
{
	if (attbyval)
	{
		switch (attlen)
		{
			case sizeof(int32):
				return Int32GetDatum(*((const int32 *) T));
			case sizeof(int16):
				return Int16GetDatum(*((const int16 *) T));
			case sizeof(char):
				return CharGetDatum(*((const char *) T));
			default:
				Assert(attlen == sizeof(int64));
				return Int64GetDatum(*((const int64 *) T));
		}
	}
	else
		return PointerGetDatum(T);
}

/*
 * Align *off for an attribute, fetch the attribute at tupptr + *off and
 * move *off past it.
 */
static inline Datum
align_fetch_then_add(const char *tupptr, uint32 *off, bool attbyval, int attlen,
					 uint8 attalignby)
{
	Datum		res;

	if (attlen > 0)
	{
		const char *offset_ptr;

		*off = TYPEALIGN(attalignby, *off);
		offset_ptr = tupptr + *off;
		*off += attlen;
		if (attbyval)
		{
			switch (attlen)
			{
				case sizeof(char):
					return CharGetDatum(*((const char *) offset_ptr));
				case sizeof(int16):
					return Int16GetDatum(*((const int16 *) offset_ptr));
				case sizeof(int32):
					return Int32GetDatum(*((const int32 *) offset_ptr));
				default:
					Assert(attlen == sizeof(int64));
					return Int64GetDatum(*((const int64 *) offset_ptr));
			}
		}
		return PointerGetDatum(offset_ptr);
	}
	else if (attlen == -1)
	{
		if (!VARATT_IS_SHORT(tupptr + *off))
			*off = TYPEALIGN(attalignby, *off);

		res = PointerGetDatum(tupptr + *off);
		*off += VARSIZE_ANY(DatumGetPointer(res));
		return res;
	}
	else
	{
		Assert(attlen == -2);
		*off = TYPEALIGN(attalignby, *off);
		res = PointerGetDatum(tupptr + *off);
		*off += strlen(tupptr + *off) + 1;
		return res;
	}
}

/*
 * The 0-based number of the first NULL attribute in a NULL bitmap, or
 * natts when none comes before it. bits must hold a 0 bit somewhere, as
 * the bitmap of a tuple with NULLs does.
 */
static inline int
first_null_attr(const uint8 *bits, int natts)
{
	int			nattByte = natts >> 3;
	int			bytenum;
	int			res;

#ifdef USE_ASSERT_CHECKING
	int			firstnull_check = natts;

	for (int i = 0; i < natts; i++)
	{
		if (att_isnull(i, bits))
		{
			firstnull_check = i;
			break;
		}
	}
#endif

	for (bytenum = 0; bytenum < nattByte; bytenum++)
	{
		if (bits[bytenum] != 0xFF)
			break;
	}
	/* The bits past natts may give more than natts; Min caps it. */
	res = bytenum << 3;
	res += pg_rightmost_one_pos32(~((uint32) bits[bytenum]));
	res = Min(res, natts);
	Assert(res == firstnull_check);
	return res;
}
#endif

/*
 * The leading attributes at cached offsets, as the descriptor counts them.
 * PostgreSQL 18 keeps no such count and sets its attcacheoff values as
 * tuples of the descriptor are deformed, not all ahead of them, so there
 * the cursor walks from its place.
 */
#if PG_VERSION_NUM >= 190000
#define TESS_DEFORM_CACHED_ATTRS(tuple_desc) ((tuple_desc)->firstNonCachedOffsetAttr)
#else
#define TESS_DEFORM_CACHED_ATTRS(tuple_desc) 0
#endif

/*
 * A batch provider that keeps heap tuples deforms a column only when a
 * consumer asks for it, so a later column of a row must be reachable from
 * where an earlier request stopped, without walking the row again. The
 * cursor below remembers that place per tuple: the attributes passed so
 * far and the byte offset in the tuple data. It is the deformation of
 * PostgreSQL's own slots (slot_deform_heap_tuple) reduced to one target
 * attribute and made resumable, ported from pg_batch's heap_deform.h; it
 * relies only on the public inline helpers of access/tupmacs.h and on the
 * compact attribute metadata of the tuple descriptor, so PostgreSQL itself
 * is not patched. See docs/runtime.md.
 */
typedef struct TessDeformCursor
{
	/* Attributes passed so far, one-based high-water mark. */
	AttrNumber	nvalid;
	/* Byte offset in the tuple data after the last attribute passed. */
	uint32		off;
} TessDeformCursor;

static inline void
tess_deform_cursor_init(TessDeformCursor *cursor)
{
	cursor->nvalid = 0;
	cursor->off = 0;
}

/*
 * Advance the cursor to one physical attribute of the tuple and fetch it,
 * without materializing the attributes in between. target_attnum is
 * one-based and must lie beyond the cursor's high-water mark; the cursor
 * may only be reused with the same tuple, descriptor and
 * first_non_guaranteed_attr. A by-reference Datum points into the tuple.
 * Returns false, leaving value and isnull unset, when the physical tuple
 * has fewer attributes than target_attnum: the caller then supplies the
 * missing value, as getmissingattr does.
 *
 * first_non_guaranteed_attr is the number of leading attributes that every
 * tuple is known to have present, non-NULL and passed by value, as the
 * descriptor's firstNonGuaranteedAttr or a slot's tts_first_nonguaranteed
 * says; a caller without that knowledge passes 0, as on PostgreSQL 18,
 * which has neither. support_cstring is true
 * only for descriptors that may hold cstring attributes, and should be a
 * compile-time constant.
 */
static pg_always_inline bool
tess_deform_advance(TessDeformCursor *cursor, HeapTuple tuple,
					TupleDesc tuple_desc, int first_non_guaranteed_attr,
					AttrNumber target_attnum, Datum *value, bool *isnull,
					bool support_cstring)
{
	CompactAttribute *cattrs = tuple_desc->compact_attrs;
	CompactAttribute *cattr;
	HeapTupleHeader tup = tuple->t_data;
	size_t		attnum = cursor->nvalid;
	int			first_non_cache_offset_attr;
	int			first_null_attr_num;
	int			natts;
	char	   *tp = (char *) tup + tup->t_hoff;
	uint32		off = cursor->off;

	Assert(TESS_DEFORM_CACHED_ATTRS(tuple_desc) >= 0);
#if PG_VERSION_NUM < 190000
	/*
	 * 18 sets its attcacheoff values as tuples are deformed, not all ahead of
	 * them, so no prefix is taken as guaranteed there, whatever the caller
	 * passes.
	 */
	first_non_guaranteed_attr = 0;
#endif
	Assert(target_attnum > 0 && target_attnum <= tuple_desc->natts);
	Assert(cursor->nvalid >= 0 && cursor->nvalid < target_attnum);
	Assert(first_non_guaranteed_attr >= 0 &&
		   first_non_guaranteed_attr <= tuple_desc->natts);
	Assert(off == 0 || attnum > 0);

	first_non_guaranteed_attr = Min(target_attnum, first_non_guaranteed_attr);

	/* All attributes in this prefix have fixed, cached offsets. */
	if (target_attnum <= first_non_guaranteed_attr)
	{
		cattr = &cattrs[target_attnum - 1];
		Assert(cattr->attcacheoff >= 0);
		Assert(cattr->attlen > 0 && cattr->attbyval);
		off = cattr->attcacheoff;
		*value = fetch_att_noerr(tp + off, true, cattr->attlen);
		*isnull = false;
		cursor->nvalid = target_attnum;
		cursor->off = off + cattr->attlen;
		return true;
	}

	natts = Min(HeapTupleHeaderGetNatts(tup), target_attnum);
	if (attnum >= natts)
		return false;

	if (HeapTupleHasNulls(tuple))
		first_null_attr_num = first_null_attr(tup->t_bits, natts);
	else
		first_null_attr_num = natts;
	first_non_cache_offset_attr =
		Min(TESS_DEFORM_CACHED_ATTRS(tuple_desc), first_null_attr_num);

	/* Skip any remaining guaranteed prefix using its last cached offset. */
	if (attnum < first_non_guaranteed_attr)
	{
		cattr = &cattrs[first_non_guaranteed_attr - 1];
		Assert(cattr->attcacheoff >= 0);
		Assert(cattr->attlen > 0 && cattr->attbyval);
		off = cattr->attcacheoff + cattr->attlen;
		attnum = first_non_guaranteed_attr;
	}

	/* Cached offsets let us jump directly to the target attribute. */
	if (attnum < first_non_cache_offset_attr)
	{
		if (target_attnum <= first_non_cache_offset_attr)
		{
			cattr = &cattrs[target_attnum - 1];
			Assert(cattr->attcacheoff >= 0 && cattr->attlen > 0);
			off = cattr->attcacheoff;
			*value = fetch_att_noerr(tp + off, cattr->attbyval, cattr->attlen);
			*isnull = false;
			cursor->nvalid = target_attnum;
			cursor->off = off + cattr->attlen;
			return true;
		}

		cattr = &cattrs[first_non_cache_offset_attr - 1];
		Assert(cattr->attcacheoff >= 0 && cattr->attlen > 0);
		off = cattr->attcacheoff + cattr->attlen;
		attnum = first_non_cache_offset_attr;
	}

	/* Walk non-cached attributes before the first NULL. */
	for (; attnum < first_null_attr_num; attnum++)
	{
		Datum		datum;

		cattr = &cattrs[attnum];
		if (!support_cstring)
			pg_assume(cattr->attlen > 0 || cattr->attlen == -1);
		datum = align_fetch_then_add(tp, &off, cattr->attbyval,
									 cattr->attlen, cattr->attalignby);
		if (attnum == target_attnum - 1)
		{
			*value = datum;
			*isnull = false;
			cursor->nvalid = target_attnum;
			cursor->off = off;
			return true;
		}
	}

	/* Continue with NULL checks after the first NULL attribute. */
	for (; attnum < natts; attnum++)
	{
		Datum		datum;

		if (att_isnull(attnum, tup->t_bits))
		{
			if (attnum == target_attnum - 1)
			{
				*value = (Datum) 0;
				*isnull = true;
				cursor->nvalid = target_attnum;
				cursor->off = off;
				return true;
			}
			continue;
		}

		cattr = &cattrs[attnum];
		if (!support_cstring)
			pg_assume(cattr->attlen > 0 || cattr->attlen == -1);
		datum = align_fetch_then_add(tp, &off, cattr->attbyval,
									 cattr->attlen, cattr->attalignby);
		if (attnum == target_attnum - 1)
		{
			*value = datum;
			*isnull = false;
			cursor->nvalid = target_attnum;
			cursor->off = off;
			return true;
		}
	}

	cursor->nvalid = natts;
	cursor->off = off;
	return false;
}

#endif							/* TESSERA_HEAP_DEFORM_H */

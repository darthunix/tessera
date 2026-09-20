/* Incremental deformation of one heap tuple, attribute by attribute. */
#ifndef TESSERA_HEAP_DEFORM_H
#define TESSERA_HEAP_DEFORM_H

#include "postgres.h"

#include "access/htup_details.h"
#include "access/tupdesc.h"
#include "access/tupmacs.h"

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
 * says; a caller without that knowledge passes 0. support_cstring is true
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

	Assert(tuple_desc->firstNonCachedOffsetAttr >= 0);
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
		Min(tuple_desc->firstNonCachedOffsetAttr, first_null_attr_num);

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

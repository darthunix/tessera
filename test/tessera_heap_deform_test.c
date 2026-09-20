#include "postgres.h"

#include "access/relscan.h"
#include "access/table.h"
#include "access/tableam.h"
#include "executor/tuptable.h"
#include "fmgr.h"
#include "utils/datum.h"
#include "utils/snapmgr.h"

#include "tessera/heap_deform.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_heap_deform);

/*
 * The cursor against PostgreSQL's own deformation, on every tuple of a
 * relation: attribute by attribute with one cursor, a jump straight to
 * each attribute with a fresh cursor, and a jump followed by a resumption
 * to every later attribute; each with three guaranteed prefixes.
 */
static void
check(TupleTableSlot *slot, HeapTuple tuple, int guaranteed, AttrNumber att,
	  TessDeformCursor *cursor, const char *how)
{
	TupleDesc	desc = slot->tts_tupleDescriptor;
	Form_pg_attribute attr = TupleDescAttr(desc, att - 1);
	Datum		value = (Datum) 0;
	bool		isnull = false;
	Datum		expected = slot->tts_values[att - 1];
	bool		expected_null = slot->tts_isnull[att - 1];

	if (!tess_deform_advance(cursor, tuple, desc, guaranteed, att,
							 &value, &isnull, false))
		value = getmissingattr(desc, att, &isnull);
	if (isnull != expected_null ||
		(!isnull && !datumIsEqual(value, expected, attr->attbyval, attr->attlen)))
		elog(ERROR, "attribute %d differs (%s, guaranteed %d): got %s, expected %s",
			 att, how, guaranteed, isnull ? "NULL" : "a value",
			 expected_null ? "NULL" : "a value");
}

static void
check_tuple(TupleTableSlot *slot, HeapTuple tuple, int guaranteed)
{
	int			natts = slot->tts_tupleDescriptor->natts;
	TessDeformCursor cursor;

	tess_deform_cursor_init(&cursor);
	for (AttrNumber att = 1; att <= natts; att++)
		check(slot, tuple, guaranteed, att, &cursor, "in order");
	for (AttrNumber att = 1; att <= natts; att++)
	{
		tess_deform_cursor_init(&cursor);
		check(slot, tuple, guaranteed, att, &cursor, "by a jump");
		for (AttrNumber later = att + 1; later <= natts; later++)
		{
			TessDeformCursor resumed = cursor;

			check(slot, tuple, guaranteed, later, &resumed, "after a jump");
		}
	}
}

Datum
tessera_test_heap_deform(PG_FUNCTION_ARGS)
{
	Oid			relid = PG_GETARG_OID(0);
	Relation	rel = table_open(relid, AccessShareLock);
	TupleTableSlot *slot = table_slot_create(rel, NULL);
	TableScanDesc scan = table_beginscan(rel, GetActiveSnapshot(), 0, NULL, 0);
	int64		tuples = 0;

	while (table_scan_getnextslot(scan, ForwardScanDirection, slot))
	{
		BufferHeapTupleTableSlot *bslot = (BufferHeapTupleTableSlot *) slot;
		int			guaranteed[3];

		if (!TTS_IS_BUFFERTUPLE(slot))
			elog(ERROR, "the scan returned no buffer tuple");
		slot_getallattrs(slot);
		guaranteed[0] = 0;
		guaranteed[1] = slot->tts_first_nonguaranteed;
		guaranteed[2] = slot->tts_tupleDescriptor->firstNonGuaranteedAttr;
		for (int mode = 0; mode < 3; mode++)
			check_tuple(slot, bslot->base.tuple, guaranteed[mode]);
		tuples++;
	}
	table_endscan(scan);
	ExecDropSingleTupleTableSlot(slot);
	table_close(rel, AccessShareLock);
	PG_RETURN_INT64(tuples);
}

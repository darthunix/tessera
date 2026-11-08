#include "postgres.h"

#include "ao_reader.h"

#if TESS_MPP
#include "access/htup_details.h"
#include "access/tableam.h"
#include "catalog/pg_am_d.h"
#include "cdb/cdbaocsam.h"
#include "cdb/cdbappendonlyam.h"
#include "nodes/bitmapset.h"
#include "pgstat.h"
#include "port/pg_bitutils.h"
#endif
#include "utils/rel.h"

/*
 * The formats read here: those whose scan primitives the core exports,
 * as its headers announce. The code of each follows the core's own scan
 * of the format, aocs_getnext and appendonlygettup, and the example the
 * core keeps of these primitives, src/test/modules/test_ao_scan. A fork
 * whose append-only code differs gets its branch here, under the macros
 * of its own headers.
 */
#if TESS_MPP && defined(AOCS_SCAN_PRIMITIVES)
#define TESS_AO_COLUMN 1
#else
#define TESS_AO_COLUMN 0
#endif
#if TESS_MPP && defined(APPENDONLY_SCAN_PRIMITIVES)
#define TESS_AO_ROW 1
#else
#define TESS_AO_ROW 0
#endif

struct TessAoReader
{
	Relation	relation;
	/* The attributes asked for, zero-based. */
	int			nattrs;
	AttrNumber *attnos;
#if TESS_AO_COLUMN
	AOCSScanDesc aocs;			/* of an ao_column table, or */
	bool		in_segment;		/* aocs is inside a segment file */
	/* The values of a column read for the row numbers only, thrown away. */
	Datum		skip_values[TESS_AO_READER_MAX_ROWS];
	bool		skip_isnull[TESS_AO_READER_MAX_ROWS];
#endif
#if TESS_AO_ROW
	AppendOnlyScanDesc ao;		/* of an ao_row table */
#endif
	/* The segment file and the row numbers of the last batch. */
	int			segno;
	int64		rownums[TESS_AO_READER_MAX_ROWS];
};

const char *
tess_ao_reader_format(Oid relam)
{
#if TESS_AO_COLUMN
	if (relam == AO_COLUMN_TABLE_AM_OID)
		return "ao_column";
#endif
#if TESS_AO_ROW
	if (relam == AO_ROW_TABLE_AM_OID)
		return "ao_row";
#endif
	(void) relam;
	return NULL;
}

TessAoReader *
tess_ao_reader_begin(Relation relation, Snapshot snapshot, int nattrs,
					 const AttrNumber *attnos)
{
	TessAoReader *reader = palloc0(sizeof(TessAoReader));

	reader->relation = relation;
	reader->nattrs = nattrs;
	reader->attnos = palloc(sizeof(AttrNumber) * Max(nattrs, 1));
	memcpy(reader->attnos, attnos, sizeof(AttrNumber) * nattrs);
#if TESS_AO_COLUMN
	if (relation->rd_rel->relam == AO_COLUMN_TABLE_AM_OID)
	{
		bool	   *proj = palloc0(sizeof(bool) * Max(RelationGetNumberOfAttributes(relation), 1));

		for (int i = 0; i < nattrs; i++)
			proj[attnos[i]] = true;
		reader->aocs = aocs_beginscan(relation, snapshot, nattrs > 0 ? proj : NULL,
									  nattrs > 0 ? AOCS_PROJ_SOME : AOCS_PROJ_ANY,
									  SO_TYPE_SEQSCAN | SO_ALLOW_STRAT | SO_ALLOW_SYNC |
									  SO_ALLOW_PAGEMODE);
		pfree(proj);
		return reader;
	}
#endif
#if TESS_AO_ROW
	if (relation->rd_rel->relam == AO_ROW_TABLE_AM_OID)
	{
		reader->ao = (AppendOnlyScanDesc)
			appendonly_beginscan(relation, snapshot, 0, NULL, NULL,
								 SO_TYPE_SEQSCAN | SO_ALLOW_STRAT | SO_ALLOW_SYNC |
								 SO_ALLOW_PAGEMODE);
		return reader;
	}
#endif
	(void) snapshot;
	elog(ERROR, "Tessera cannot read the blocks of relation \"%s\"",
		 RelationGetRelationName(relation));
	return NULL;
}

#if TESS_AO_COLUMN || TESS_AO_ROW
/*
 * The mask of the batch's nrows rows, and the count of the rows the scan
 * returns, as the core's scans keep it. One entry of the visibility map
 * usually covers them all, and with no row deleted in it they are all
 * seen.
 */
static uint64
batch_visible(TessAoReader *reader, AppendOnlyVisimap *visimap, bool any_row, int nrows)
{
	uint64		visible = nrows == TESS_AO_READER_MAX_ROWS ? ~UINT64CONST(0) : (UINT64CONST(1) << nrows) - 1;
	PgStat_TableStatus *stats = reader->relation->pgstat_info;
	AOTupleId	first;
	AOTupleId	last;

	if (!any_row)
	{
		AOTupleIdInit(&first, reader->segno, reader->rownums[0]);
		AOTupleIdInit(&last, reader->segno, reader->rownums[nrows - 1]);
		if (!AppendOnlyVisimap_IsVisible(visimap, &first) ||
			!AppendOnlyVisimapEntry_CoversTuple(&visimap->visimapEntry, &last) ||
			!bms_is_empty(visimap->visimapEntry.bitmap))
		{
			for (int row = 0; row < nrows; row++)
			{
				AOTupleId	tid;

				AOTupleIdInit(&tid, reader->segno, reader->rownums[row]);
				if (!AppendOnlyVisimap_IsVisible(visimap, &tid))
					visible &= ~(UINT64CONST(1) << row);
			}
		}
	}
	if (stats != NULL)
		stats->t_counts.t_tuples_returned += pg_popcount64(visible);
	return visible;
}
#endif

#if TESS_AO_ROW
/*
 * The next rows of an ao_row scan. A memtuple gives any attribute at
 * once, so only those asked for are read, and an attribute added after
 * the row was written has its missing value.
 */
static int
ao_row_next(TessAoReader *reader, int maxrows, Datum **values, bool **isnull,
			uint64 *visible)
{
	AppendOnlyScanDesc ao = reader->ao;
	AppendOnlyExecutorReadBlock *block = &ao->executorReadBlock;
	TupleDesc	tupdesc = RelationGetDescr(reader->relation);

	for (;;)
	{
		int			nrows = 0;

		if (ao->needNextBuffer && !appendonly_scan_next_block(ao))
			return 0;
		while (nrows < maxrows)
		{
			MemTuple	tuple;
			int32		tuple_len;

			if (!AppendOnlyExecutorReadBlock_NextTuple(block, &tuple, &reader->rownums[nrows],
													   &tuple_len))
			{
				ao->needNextBuffer = true;
				break;
			}
			AppendOnlyExecutorReadBlock_BindingInit(block, tupdesc, reader->rownums[nrows]);
			for (int i = 0; i < reader->nattrs; i++)
			{
				int			attno = reader->attnos[i];

				if (attno < block->mt_bind->natts)
					values[attno][nrows] =
						memtuple_getattr_inline(tuple, block->mt_bind, attno + 1,
												&isnull[attno][nrows]);
				else
					values[attno][nrows] =
						getmissingattr(tupdesc, attno + 1, &isnull[attno][nrows]);
			}
			nrows++;
		}
		if (nrows == 0)
			continue;
		reader->segno = block->segmentFileNum;
		*visible = batch_visible(reader, &ao->visibilityMap, ao->snapshot == SnapshotAny,
								 nrows);
		return nrows;
	}
}
#endif

#if TESS_AO_COLUMN
/* The rows of a column's current block not read yet. */
static inline int
rows_left_in_block(DatumStreamRead *ds)
{
	if (ds->largeObjectState == DatumStreamLargeObjectState_None)
		return Max(ds->blockRead.logical_row_count - (ds->blockRead.nth + 1), 0);
	/* A large value fills its blocks alone: one row, until read. */
	return ds->largeObjectState == DatumStreamLargeObjectState_HaveAoContent ? 1 : 0;
}

/*
 * Make a column's current block hold a row not read yet, reading its
 * next block when the current one is done. False when the segment file
 * has no more blocks of the column.
 */
static bool
column_has_row(AOCSScanDesc aocs, AttrNumber attno)
{
	DatumStreamRead *ds = aocs->columnScanInfo.ds[attno];

	if (rows_left_in_block(ds) > 0)
		return true;
	(void) datumstreamread_advance(ds);
	if (datumstreamread_block(ds, aocs->blockDirectory, attno) < 0)
		return false;
	AOCSScanDesc_UpdateTotalBytesRead(aocs, attno);
	pgstat_count_buffer_read_ao(aocs->rs_base.rs_rd,
								RelationGuessNumberOfBlocksFromSize(aocs->totalBytesRead));
	return true;
}

/*
 * The next nrows values of a column's block, when it keeps them one
 * after another: a fixed width passed by value, no NULL, no RLE or delta
 * encoding, no large object. Loads them as fetch_att does, and leaves
 * the reader where advancing and getting row by row would, at the last
 * value read. False, nothing read, otherwise; a block of a version not
 * known here is among them.
 */
static bool
read_dense(DatumStreamRead *ds, int nrows, Datum *values, bool *isnull)
{
	DatumStreamBlockRead *dsr = &ds->blockRead;
	int			len = dsr->typeInfo.datumlen;
	uint8	   *first;

	if (ds->largeObjectState != DatumStreamLargeObjectState_None ||
		!dsr->typeInfo.byval || dsr->has_null ||
		(len != 1 && len != 2 && len != 4 && len != 8) ||
		dsr->nth + nrows >= dsr->logical_row_count)
		return false;
	switch (dsr->datumStreamVersion)
	{
		case DatumStreamVersion_Original:
			break;
		case DatumStreamVersion_Dense:
		case DatumStreamVersion_Dense_Enhanced:
			if (dsr->rle_block_was_compressed || dsr->delta_block_was_compressed)
				return false;
			break;
		default:
			return false;
	}
	/* The block's first value is where the read left the pointer, the others past the current one. */
	first = dsr->datump + (dsr->physical_datum_index >= 0 ? len : 0);
	switch (len)
	{
		case 1:
			for (int row = 0; row < nrows; row++)
				values[row] = CharGetDatum(((char *) first)[row]);
			break;
		case 2:
			for (int row = 0; row < nrows; row++)
				values[row] = Int16GetDatum(((int16 *) first)[row]);
			break;
		case 4:
			for (int row = 0; row < nrows; row++)
				values[row] = Int32GetDatum(((int32 *) first)[row]);
			break;
		default:
			for (int row = 0; row < nrows; row++)
				values[row] = ((Datum *) first)[row];
			break;
	}
	memset(isnull, 0, sizeof(bool) * nrows);
	dsr->datump = first + (nrows - 1) * len;
	dsr->physical_datum_index += nrows;
	dsr->nth += nrows;
	return true;
}

/*
 * The next rows of an ao_column scan. The scan reads the columns of its
 * projection, and the anchor column, which gives the rows' numbers, even
 * when nobody asked for it: that column's values go to the reader's own
 * arrays, which a dense block fills in one loop, faster than moving on
 * row by row.
 * A batch ends where a column's block ends or where a column added later
 * starts to hold values of its own.
 */
static int
ao_column_next(TessAoReader *reader, int maxrows, Datum **values, bool **isnull,
			   uint64 *visible)
{
	AOCSScanDesc aocs = reader->aocs;

	for (;;)
	{
		AttrNumber	anchor;
		DatumStreamRead *anchor_ds;
		int			segno;
		bool		missing[MaxHeapAttributeNumber];
		int64		first_row;
		int			nrows = maxrows;
		bool		segment_done = false;

		if (!reader->in_segment)
		{
			if (!aocs_scan_next_seg(aocs))
				return 0;
			reader->in_segment = true;
		}
		anchor = aocs->columnScanInfo.proj_atts[ANCHOR_COL_IN_PROJ];
		anchor_ds = aocs->columnScanInfo.ds[anchor];
		segno = aocs->seginfo[aocs->cur_seg]->segno;
		if (!column_has_row(aocs, anchor))
		{
			reader->in_segment = false;
			continue;
		}
		/* A large value, alone in its blocks, is its block's first row. */
		if (anchor_ds->blockFirstRowNum != InvalidAORowNum)
			first_row = anchor_ds->blockFirstRowNum +
				(anchor_ds->largeObjectState == DatumStreamLargeObjectState_None ?
				 anchor_ds->blockRead.nth + 1 : 0);
		else
			first_row = aocs->segrowsprocessed + 1;
		nrows = Min(nrows, rows_left_in_block(anchor_ds));

		for (AttrNumber i = 0; i < aocs->columnScanInfo.num_proj_atts; i++)
		{
			AttrNumber	attno = aocs->columnScanInfo.proj_atts[i];
			int64		last_missing;

			missing[attno] = false;
			if (attno == anchor)
				continue;
			last_missing = aocs->columnScanInfo.attnum_to_rownum[attno * MAX_AOREL_CONCURRENCY +
																 segno];
			if (anchor_ds->blockFirstRowNum != InvalidAORowNum && first_row <= last_missing)
			{
				missing[attno] = true;
				nrows = (int) Min((int64) nrows, last_missing - first_row + 1);
				continue;
			}
			if (!column_has_row(aocs, attno))
			{
				segment_done = true;
				break;
			}
			nrows = Min(nrows, rows_left_in_block(aocs->columnScanInfo.ds[attno]));
		}
		if (segment_done)
		{
			reader->in_segment = false;
			continue;
		}

		for (AttrNumber i = 0; i < aocs->columnScanInfo.num_proj_atts; i++)
		{
			AttrNumber	attno = aocs->columnScanInfo.proj_atts[i];
			DatumStreamRead *ds = aocs->columnScanInfo.ds[attno];
			Datum	   *column = values[attno];
			bool	   *nulls = isnull[attno];

			if (column == NULL)
			{
				column = reader->skip_values;
				nulls = reader->skip_isnull;
			}
			if (missing[attno])
			{
				bool		value_null;
				Datum		value = getmissingattr(RelationGetDescr(reader->relation), attno + 1,
												   &value_null);

				for (int row = 0; row < nrows; row++)
				{
					column[row] = value;
					nulls[row] = value_null;
				}
			}
			else if (!read_dense(ds, nrows, column, nulls))
			{
				for (int row = 0; row < nrows; row++)
				{
					(void) datumstreamread_advance(ds);
					/* The reader writes no Datum for a NULL. */
					column[row] = (Datum) 0;
					datumstreamread_get(ds, &column[row], &nulls[row]);
				}
			}
		}

		for (int row = 0; row < nrows; row++)
			reader->rownums[row] = first_row + row;
		reader->segno = segno;
		*visible = batch_visible(reader, &aocs->visibilityMap,
								 aocs->rs_base.rs_snapshot == SnapshotAny, nrows);
		aocs->segrowsprocessed += nrows;
		return nrows;
	}
}
#endif

int
tess_ao_reader_next(TessAoReader *reader, int maxrows, Datum **values, bool **isnull,
					uint64 *visible)
{
	Assert(maxrows > 0 && maxrows <= TESS_AO_READER_MAX_ROWS);
#if TESS_AO_COLUMN
	if (reader->aocs != NULL)
		return ao_column_next(reader, maxrows, values, isnull, visible);
#endif
#if TESS_AO_ROW
	if (reader->ao != NULL)
		return ao_row_next(reader, maxrows, values, isnull, visible);
#endif
	(void) reader;
	(void) maxrows;
	(void) values;
	(void) isnull;
	(void) visible;
	elog(ERROR, "Tessera append-only reader has no scan");
	return 0;
}

void
tess_ao_reader_rescan(TessAoReader *reader)
{
#if TESS_AO_COLUMN
	if (reader->aocs != NULL)
	{
		aocs_rescan(reader->aocs);
		reader->in_segment = false;
	}
#endif
#if TESS_AO_ROW
	if (reader->ao != NULL)
		appendonly_rescan((TableScanDesc) reader->ao, NULL, false, false, false, false);
#endif
	(void) reader;
}

void
tess_ao_reader_end(TessAoReader *reader)
{
#if TESS_AO_COLUMN
	if (reader->aocs != NULL)
		aocs_endscan(reader->aocs);
#endif
#if TESS_AO_ROW
	if (reader->ao != NULL)
		appendonly_endscan((TableScanDesc) reader->ao);
#endif
	pfree(reader->attnos);
	pfree(reader);
}

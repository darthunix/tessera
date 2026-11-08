/*
 * A reader of append-only tables by blocks, without a tuple slot: the
 * rows of a block come at once, column by column, with a mask of the
 * rows the snapshot sees.
 *
 * Append-only tables are the Greenplum family's, and a fork's code that
 * reads them is its own: all of it is in ao_reader.c, which reads a
 * format only where the core exports the primitives of its scan and
 * announces them in its headers. Elsewhere tess_ao_reader_format is NULL
 * for every table and the other calls are errors, so a caller needs no
 * test of the core of its own.
 */
#ifndef TESSERA_AO_READER_H
#define TESSERA_AO_READER_H

#include "utils/relcache.h"
#include "utils/snapshot.h"

/* The rows a batch has at most: the bits of its mask. */
#define TESS_AO_READER_MAX_ROWS 64

typedef struct TessAoReader TessAoReader;

/*
 * The name of the format of the tables of access method relam, as
 * EXPLAIN shows it, when the reader can read them on this core; NULL
 * otherwise. The name is a constant.
 */
extern const char *tess_ao_reader_format(Oid relam);

/*
 * Begin a forward scan of the attributes attnos[0 .. nattrs - 1]
 * (zero-based, not dropped; one may repeat) of a table whose format the
 * reader names, with snapshot. With no attribute the scan reads one
 * column, of its own choice, to count the rows. The reader is allocated
 * in the current memory context and copies attnos; the relation and the
 * snapshot must outlive it.
 */
extern TessAoReader *tess_ao_reader_begin(Relation relation, Snapshot snapshot,
										  int nattrs, const AttrNumber *attnos);

/*
 * The next rows of the scan, at most maxrows, 1 to
 * TESS_AO_READER_MAX_ROWS: attribute attno of row r goes to
 * values[attno][r] and isnull[attno][r], arrays the caller gives for
 * each attribute of the scan (any other may be NULL). *visible gets a
 * bit for each row the snapshot sees; the others are deleted rows,
 * whose values are filled too. A value passed by reference points into
 * a block the reader keeps until the next call, a rescan or the end, so
 * a batch never spans two blocks of a column. Returns the number of
 * rows, 0 at the end of the scan.
 */
extern int	tess_ao_reader_next(TessAoReader *reader, int maxrows,
								Datum **values, bool **isnull, uint64 *visible);

/* Start the scan anew, from its first row. */
extern void tess_ao_reader_rescan(TessAoReader *reader);

/* End the scan and free the reader. */
extern void tess_ao_reader_end(TessAoReader *reader);

#endif							/* TESSERA_AO_READER_H */

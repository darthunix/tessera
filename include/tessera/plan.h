/* Named, versioned storage of node data in custom_private lists. */
#ifndef TESSERA_PLAN_H
#define TESSERA_PLAN_H

#include "postgres.h"

#include "nodes/bitmapset.h"
#include "nodes/pg_list.h"

/*
 * A CustomPath or CustomScan carries node data in a List that PostgreSQL
 * copies with copyObject and serializes with nodeToString, for cached plans
 * and parallel workers. The codec stores that data as named, typed fields
 * behind a record kind and version: (String kind, Integer version, List of
 * (String name, value)). A writer refuses duplicate names; a reader refuses
 * another kind or version, malformed or duplicate fields, a missing field,
 * a field of another type, a field read twice and, when finished, a field
 * that was never read, so that a node never silently ignores its own data.
 * Increment the version when required fields or their meaning change. See
 * docs/runtime.md.
 */
typedef struct TessPlanWriter TessPlanWriter;
typedef struct TessPlanReader TessPlanReader;

/* Start a record of the given nonempty kind and positive version. */
extern TessPlanWriter *tess_plan_writer_create(const char *kind, int version);

extern void tess_plan_write_int(TessPlanWriter *writer, const char *name,
								int value);
/* The string is copied; NULL is an error. */
extern void tess_plan_write_string(TessPlanWriter *writer, const char *name,
								   const char *value);
/* The node is copied with copyObject; NULL is stored as NULL. */
extern void tess_plan_write_node(TessPlanWriter *writer, const char *name,
								 const Node *value);
/* A List of nodes, copied; NIL is allowed. */
extern void tess_plan_write_list(TessPlanWriter *writer, const char *name,
								 const List *value);
/* An IntList, copied; NIL is allowed. */
extern void tess_plan_write_int_list(TessPlanWriter *writer, const char *name,
									 const List *value);
/* Stored as the IntList of its members; NULL is the empty set. */
extern void tess_plan_write_bitmap(TessPlanWriter *writer, const char *name,
								   const Bitmapset *value);

/* Return the record; the writer accepts nothing afterwards. */
extern List *tess_plan_writer_finish(TessPlanWriter *writer);

/* The kind of a record, or NULL when the header is not a record; no error. */
extern const char *tess_plan_data_kind(const List *data);

/* Check the header and the shape of every field. The data is borrowed. */
extern TessPlanReader *tess_plan_reader_create(const List *data,
											   const char *kind, int version);

/* Each read returns the stored value, borrowed from the record. */
extern int tess_plan_read_int(TessPlanReader *reader, const char *name);
extern const char *tess_plan_read_string(TessPlanReader *reader,
										 const char *name);
extern Node *tess_plan_read_node(TessPlanReader *reader, const char *name);
extern List *tess_plan_read_list(TessPlanReader *reader, const char *name);
extern List *tess_plan_read_int_list(TessPlanReader *reader, const char *name);
/* A new set built from the stored members. */
extern Bitmapset *tess_plan_read_bitmap(TessPlanReader *reader,
										const char *name);

/* Raise an error naming the first field that was not read. */
extern void tess_plan_reader_finish(TessPlanReader *reader);

#endif							/* TESSERA_PLAN_H */

#include "postgres.h"

#include <string.h>

#include "catalog/pg_type_d.h"
#include "fmgr.h"
#include "nodes/makefuncs.h"
#include "nodes/value.h"

#include "tessera/plan.h"

PG_MODULE_MAGIC;

PG_FUNCTION_INFO_V1(tessera_test_plan_roundtrip);
PG_FUNCTION_INFO_V1(tessera_test_plan_errors);

#define KIND "test.record"
#define VERSION 3

/* A record with every field type, including empty values. */
static List *
make_record(void)
{
	TessPlanWriter *writer = tess_plan_writer_create(KIND, VERSION);
	Bitmapset  *set = NULL;

	set = bms_add_member(set, 2);
	set = bms_add_member(set, 5);
	set = bms_add_member(set, 64);
	tess_plan_write_int(writer, "count", 7);
	tess_plan_write_string(writer, "name", "alpha");
	tess_plan_write_node(writer, "expr",
						 (Node *) makeConst(INT4OID, -1, InvalidOid, 4,
											Int32GetDatum(42), false, true));
	tess_plan_write_node(writer, "none", NULL);
	tess_plan_write_list(writer, "items",
						 list_make2(makeString("x"), makeInteger(3)));
	tess_plan_write_list(writer, "empty_list", NIL);
	tess_plan_write_int_list(writer, "ints", list_make3_int(1, 2, 3));
	tess_plan_write_int_list(writer, "empty_ints", NIL);
	tess_plan_write_bitmap(writer, "set", set);
	tess_plan_write_bitmap(writer, "empty_set", NULL);
	return tess_plan_writer_finish(writer);
}

/* Read every field back, in another order than written. */
static bool
check_record(const List *data)
{
	TessPlanReader *reader;
	Node	   *expr;
	List	   *items;
	List	   *ints;
	Bitmapset  *set;
	bool		result = true;

	if (tess_plan_data_kind(data) == NULL ||
		strcmp(tess_plan_data_kind(data), KIND) != 0)
		return false;
	reader = tess_plan_reader_create(data, KIND, VERSION);
	set = tess_plan_read_bitmap(reader, "set");
	result &= bms_num_members(set) == 3 && bms_is_member(2, set) &&
		bms_is_member(5, set) && bms_is_member(64, set);
	result &= tess_plan_read_bitmap(reader, "empty_set") == NULL;
	ints = tess_plan_read_int_list(reader, "ints");
	result &= list_length(ints) == 3 && linitial_int(ints) == 1 &&
		lthird_int(ints) == 3;
	result &= tess_plan_read_int_list(reader, "empty_ints") == NIL;
	items = tess_plan_read_list(reader, "items");
	result &= list_length(items) == 2 && IsA(linitial(items), String) &&
		strcmp(strVal(linitial(items)), "x") == 0 &&
		IsA(lsecond(items), Integer) && intVal(lsecond(items)) == 3;
	result &= tess_plan_read_list(reader, "empty_list") == NIL;
	result &= tess_plan_read_node(reader, "none") == NULL;
	expr = tess_plan_read_node(reader, "expr");
	result &= expr != NULL && IsA(expr, Const) &&
		castNode(Const, expr)->consttype == INT4OID &&
		DatumGetInt32(castNode(Const, expr)->constvalue) == 42;
	result &= strcmp(tess_plan_read_string(reader, "name"), "alpha") == 0;
	result &= tess_plan_read_int(reader, "count") == 7;
	tess_plan_reader_finish(reader);
	return result;
}

Datum
tessera_test_plan_roundtrip(PG_FUNCTION_ARGS)
{
	List	   *data = make_record();
	bool		result;

	result = check_record(data);
	/* Cached plans copy custom_private; parallel workers read its text. */
	result &= check_record(copyObject(data));
	result &= check_record(stringToNode(nodeToString(data)));
	result &= check_record(data);
	/* Anything without the header is not a record, silently. */
	result &= tess_plan_data_kind(NIL) == NULL;
	result &= tess_plan_data_kind(list_make1(makeInteger(1))) == NULL;
	result &= tess_plan_data_kind(list_make3(makeString(KIND),
											 makeString("1"), NIL)) == NULL;
	result &= tess_plan_data_kind(list_make3(makeString(KIND),
											 makeInteger(1),
											 makeInteger(2))) == NULL;
	PG_RETURN_BOOL(result);
}

/* A record whose fields list is built by hand. */
static List *
make_raw_record(List *fields)
{
	return list_make3(makeString(KIND), makeInteger(VERSION), fields);
}

static List *
make_field(const char *name, void *value)
{
	return list_make2(makeString(pstrdup(name)), value);
}

Datum
tessera_test_plan_errors(PG_FUNCTION_ARGS)
{
	int			kind = PG_GETARG_INT32(0);
	TessPlanWriter *writer;
	TessPlanReader *reader;
	List	   *data;

	switch (kind)
	{
		case 0:
			tess_plan_writer_create("", 1);
			break;
		case 1:
			writer = tess_plan_writer_create(KIND, VERSION);
			tess_plan_write_int(writer, "count", 1);
			tess_plan_write_string(writer, "count", "one");
			break;
		case 2:
			writer = tess_plan_writer_create(KIND, VERSION);
			tess_plan_write_string(writer, "name", NULL);
			break;
		case 3:
			writer = tess_plan_writer_create(KIND, VERSION);
			tess_plan_writer_finish(writer);
			tess_plan_write_int(writer, "late", 1);
			break;
		case 4:
			writer = tess_plan_writer_create(KIND, VERSION);
			tess_plan_write_int(writer, "", 1);
			break;
		case 5:
			writer = tess_plan_writer_create(KIND, VERSION);
			tess_plan_write_list(writer, "items", list_make1_int(1));
			break;
		case 6:
			writer = tess_plan_writer_create(KIND, VERSION);
			tess_plan_write_int_list(writer, "ints",
									 list_make1(makeInteger(1)));
			break;
		case 7:
			tess_plan_reader_create(make_record(), "other.record", VERSION);
			break;
		case 8:
			tess_plan_reader_create(make_record(), KIND, VERSION + 1);
			break;
		case 9:
			tess_plan_reader_create(list_make1(makeInteger(1)), KIND,
									VERSION);
			break;
		case 10:
			data = make_raw_record(list_make1(makeInteger(1)));
			tess_plan_reader_create(data, KIND, VERSION);
			break;
		case 11:
			data = make_raw_record(list_make2(make_field("a", makeInteger(1)),
											  make_field("a", makeInteger(2))));
			tess_plan_reader_create(data, KIND, VERSION);
			break;
		case 12:
			reader = tess_plan_reader_create(make_record(), KIND, VERSION);
			tess_plan_read_int(reader, "count");
			tess_plan_read_int(reader, "count");
			break;
		case 13:
			reader = tess_plan_reader_create(make_record(), KIND, VERSION);
			tess_plan_read_int(reader, "absent");
			break;
		case 14:
			reader = tess_plan_reader_create(make_record(), KIND, VERSION);
			tess_plan_read_int(reader, "name");
			break;
		case 15:
			reader = tess_plan_reader_create(make_record(), KIND, VERSION);
			tess_plan_read_string(reader, "count");
			break;
		case 16:
			reader = tess_plan_reader_create(make_record(), KIND, VERSION);
			tess_plan_read_list(reader, "count");
			break;
		case 17:
			reader = tess_plan_reader_create(make_record(), KIND, VERSION);
			tess_plan_read_int_list(reader, "items");
			break;
		case 18:
			/* A NULL value is a valid field, but not an integer. */
			data = make_raw_record(list_make1(make_field("n", NULL)));
			reader = tess_plan_reader_create(data, KIND, VERSION);
			tess_plan_read_int(reader, "n");
			break;
		case 19:
			reader = tess_plan_reader_create(make_record(), KIND, VERSION);
			tess_plan_read_int(reader, "count");
			tess_plan_reader_finish(reader);
			break;
		default:
			elog(ERROR, "unknown error case %d", kind);
	}
	PG_RETURN_VOID();
}

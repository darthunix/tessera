#include "postgres.h"

#include "nodes/makefuncs.h"
#include "nodes/value.h"

#include "tessera/plan.h"

struct TessPlanWriter
{
	char	   *kind;
	int			version;
	List	   *fields;
	bool		finished;
};

struct TessPlanReader
{
	const char *kind;
	List	   *fields;
	List	   *read_names;
};

static bool
string_list_member(List *items, const char *value)
{
	foreach_ptr(char, item, items)
	{
		if (strcmp(item, value) == 0)
			return true;
	}
	return false;
}

static void
check_name(const char *name)
{
	if (name == NULL || name[0] == '\0')
		elog(ERROR, "Tessera plan field requires a name");
}

/* True when a field is a (String name, value) pair. */
static bool
field_is_well_formed(const List *field)
{
	return field != NIL && IsA(field, List) && list_length(field) == 2 &&
		linitial(field) != NULL && IsA(linitial(field), String);
}

static List *
find_field(List *fields, const char *name)
{
	foreach_ptr(List, field, fields)
	{
		if (!field_is_well_formed(field))
			elog(ERROR, "Tessera received malformed plan data");
		if (strcmp(strVal(linitial(field)), name) == 0)
			return field;
	}
	return NIL;
}

static void
write_field(TessPlanWriter *writer, const char *name, void *value)
{
	check_name(name);
	if (writer == NULL || writer->finished)
		elog(ERROR, "Tessera plan writer is not active");
	if (find_field(writer->fields, name) != NIL)
		elog(ERROR, "Tessera plan field \"%s\" is duplicated", name);
	writer->fields = lappend(writer->fields,
							 list_make2(makeString(pstrdup(name)), value));
}

TessPlanWriter *
tess_plan_writer_create(const char *kind, int version)
{
	TessPlanWriter *writer;

	if (kind == NULL || kind[0] == '\0' || version <= 0)
		elog(ERROR, "Tessera plan writer requires a kind and version");
	writer = palloc0_object(TessPlanWriter);
	writer->kind = pstrdup(kind);
	writer->version = version;
	return writer;
}

void
tess_plan_write_int(TessPlanWriter *writer, const char *name, int value)
{
	write_field(writer, name, makeInteger(value));
}

void
tess_plan_write_string(TessPlanWriter *writer, const char *name,
					   const char *value)
{
	if (value == NULL)
		elog(ERROR, "Tessera plan string field \"%s\" is NULL", name);
	write_field(writer, name, makeString(pstrdup(value)));
}

void
tess_plan_write_node(TessPlanWriter *writer, const char *name,
					 const Node *value)
{
	write_field(writer, name, copyObject(value));
}

void
tess_plan_write_list(TessPlanWriter *writer, const char *name,
					 const List *value)
{
	if (value != NIL && !IsA(value, List))
		elog(ERROR, "Tessera plan field \"%s\" is not a List", name);
	write_field(writer, name, copyObject(value));
}

void
tess_plan_write_int_list(TessPlanWriter *writer, const char *name,
						 const List *value)
{
	if (value != NIL && !IsA(value, IntList))
		elog(ERROR, "Tessera plan field \"%s\" is not an IntList", name);
	write_field(writer, name, copyObject(value));
}

void
tess_plan_write_bitmap(TessPlanWriter *writer, const char *name,
					   const Bitmapset *value)
{
	List	   *members = NIL;
	int			member = -1;

	while ((member = bms_next_member(value, member)) >= 0)
		members = lappend_int(members, member);
	write_field(writer, name, members);
}

List *
tess_plan_writer_finish(TessPlanWriter *writer)
{
	if (writer == NULL || writer->finished)
		elog(ERROR, "Tessera plan writer is not active");
	writer->finished = true;
	return list_make3(makeString(pstrdup(writer->kind)),
					  makeInteger(writer->version), writer->fields);
}

const char *
tess_plan_data_kind(const List *data)
{
	if (data == NIL || !IsA(data, List) || list_length(data) != 3 ||
		linitial(data) == NULL || !IsA(linitial(data), String) ||
		lsecond(data) == NULL || !IsA(lsecond(data), Integer) ||
		(lthird(data) != NIL && !IsA(lthird(data), List)))
		return NULL;
	return strVal(linitial(data));
}

TessPlanReader *
tess_plan_reader_create(const List *data, const char *kind, int version)
{
	TessPlanReader *reader;
	const char *data_kind;
	List	   *fields;

	if (kind == NULL || kind[0] == '\0' || version <= 0)
		elog(ERROR, "Tessera plan reader requires a kind and version");
	data_kind = tess_plan_data_kind(data);
	if (data_kind == NULL)
		elog(ERROR, "Tessera received malformed %s plan data", kind);
	if (strcmp(data_kind, kind) != 0)
		elog(ERROR, "Tessera expected %s plan data, got %s", kind, data_kind);
	if (intVal(lsecond(data)) != version)
		elog(ERROR, "Tessera expected %s plan version %d, got %d", kind,
			 version, intVal(lsecond(data)));
	fields = lthird(data);
	foreach_ptr(List, field, fields)
	{
		const char *name;

		if (!field_is_well_formed(field))
			elog(ERROR, "Tessera received malformed %s plan field", kind);
		name = strVal(linitial(field));
		foreach_ptr(List, other, fields)
		{
			if (other == field)
				break;
			if (strcmp(strVal(linitial(other)), name) == 0)
				elog(ERROR, "Tessera %s plan field \"%s\" is duplicated",
					 kind, name);
		}
	}
	reader = palloc0_object(TessPlanReader);
	reader->kind = kind;
	reader->fields = fields;
	return reader;
}

static void *
read_field(TessPlanReader *reader, const char *name)
{
	List	   *field;

	check_name(name);
	if (reader == NULL)
		elog(ERROR, "Tessera plan reader is not active");
	if (string_list_member(reader->read_names, name))
		elog(ERROR, "Tessera %s plan field \"%s\" was read twice",
			 reader->kind, name);
	field = find_field(reader->fields, name);
	if (field == NIL)
		elog(ERROR, "Tessera %s plan field \"%s\" is missing",
			 reader->kind, name);
	reader->read_names = lappend(reader->read_names, pstrdup(name));
	return lsecond(field);
}

int
tess_plan_read_int(TessPlanReader *reader, const char *name)
{
	Node	   *value = read_field(reader, name);

	if (value == NULL || !IsA(value, Integer))
		elog(ERROR, "Tessera %s plan field \"%s\" is not an integer",
			 reader->kind, name);
	return intVal(value);
}

const char *
tess_plan_read_string(TessPlanReader *reader, const char *name)
{
	Node	   *value = read_field(reader, name);

	if (value == NULL || !IsA(value, String))
		elog(ERROR, "Tessera %s plan field \"%s\" is not a string",
			 reader->kind, name);
	return strVal(value);
}

Node *
tess_plan_read_node(TessPlanReader *reader, const char *name)
{
	return read_field(reader, name);
}

List *
tess_plan_read_list(TessPlanReader *reader, const char *name)
{
	List	   *value = read_field(reader, name);

	if (value != NIL && !IsA(value, List))
		elog(ERROR, "Tessera %s plan field \"%s\" is not a List",
			 reader->kind, name);
	return value;
}

List *
tess_plan_read_int_list(TessPlanReader *reader, const char *name)
{
	List	   *value = read_field(reader, name);

	if (value != NIL && !IsA(value, IntList))
		elog(ERROR, "Tessera %s plan field \"%s\" is not an IntList",
			 reader->kind, name);
	return value;
}

Bitmapset *
tess_plan_read_bitmap(TessPlanReader *reader, const char *name)
{
	List	   *members = tess_plan_read_int_list(reader, name);
	Bitmapset  *result = NULL;

	foreach_int(member, members)
		result = bms_add_member(result, member);
	return result;
}

void
tess_plan_reader_finish(TessPlanReader *reader)
{
	if (reader == NULL)
		elog(ERROR, "Tessera plan reader is not active");
	if (list_length(reader->read_names) == list_length(reader->fields))
		return;
	foreach_ptr(List, field, reader->fields)
	{
		const char *name = strVal(linitial(field));

		if (!string_list_member(reader->read_names, name))
			elog(ERROR, "Tessera %s plan field \"%s\" was not read",
				 reader->kind, name);
	}
}
